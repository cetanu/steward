use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cadence::{NopMetricSink, StatsdClient};
use redis::Script;
use redis::aio::ConnectionManager;
use tokio::sync::watch::Receiver;
use tonic::Response;
use tracing::{debug, error};

use crate::metrics::{ErrorRateLimiter, SharedMetrics, count, gauge, time};
use crate::proto::envoy::service::ratelimit::v3::rate_limit_response::{Code, DescriptorStatus};
use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;
use crate::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};
use crate::rate_limits::{Algorithm, RateLimit, encode_canonical_path, rate_limit_key};
use crate::response::build_response;

pub use crate::config_source::CompiledConfig;

pub type RateLimitConfigs = Arc<CompiledConfig>;

pub const DEFAULT_EXECUTION_TIMEOUT: Duration = Duration::from_millis(10);
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 1024;

/// Parse the standard gRPC timeout header (`grpc-timeout` from `request.metadata()`).
/// Standard gRPC timeouts use format `<value><unit>` where unit is `H` (hours),
/// `M` (minutes), `S` (seconds), `m` (milliseconds), `u` (microseconds), or `n` (nanoseconds).
pub fn parse_grpc_timeout(val: &str) -> Option<Duration> {
    let val = val.trim();
    if val.is_empty() {
        return None;
    }
    let mut chars = val.char_indices();
    let (last_idx, last_char) = chars.next_back()?;
    let num_part = &val[..last_idx];
    if num_part.is_empty() {
        return None;
    }
    let num: u64 = num_part.parse().ok()?;
    match last_char {
        'H' => num.checked_mul(3600).map(Duration::from_secs),
        'M' => num.checked_mul(60).map(Duration::from_secs),
        'S' => Some(Duration::from_secs(num)),
        'm' => Some(Duration::from_millis(num)),
        'u' => Some(Duration::from_micros(num)),
        'n' => Some(Duration::from_nanos(num)),
        _ => None,
    }
}

const FIXED_WINDOW_SCRIPT: &str = include_str!("scripts/fixed_window.lua");
const FIXED_WINDOW_REFUND_SCRIPT: &str = include_str!("scripts/fixed_window_refund.lua");
const TOKEN_BUCKET_SCRIPT: &str = include_str!("scripts/token_bucket.lua");
const TOKEN_BUCKET_REFUND_SCRIPT: &str = include_str!("scripts/token_bucket_refund.lua");
const SLIDING_WINDOW_SCRIPT: &str = include_str!("scripts/sliding_window.lua");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptOutcome {
    pub allowed: bool,
    pub observed: i64,
}

impl redis::FromRedisValue for ScriptOutcome {
    fn from_redis_value(v: redis::Value) -> Result<Self, redis::ParsingError> {
        let (allowed_code, observed): (i64, i64) = redis::FromRedisValue::from_redis_value(v)?;
        Ok(Self {
            allowed: allowed_code == 1,
            observed,
        })
    }
}

pub fn generate_sliding_window_nonce() -> String {
    use aws_lc_rs::rand::SecureRandom;
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes)
        .expect("system randomness failed to generate nonce");
    let mut hex = String::with_capacity(32);
    for b in bytes {
        let _ = std::fmt::write(&mut hex, format_args!("{b:02x}"));
    }
    hex
}

#[derive(Clone)]
pub struct StewardScripts {
    pub fixed_window: Script,
    pub fixed_window_refund: Script,
    pub token_bucket: Script,
    pub token_bucket_refund: Script,
    pub sliding_window: Script,
}

impl Default for StewardScripts {
    fn default() -> Self {
        Self {
            fixed_window: Script::new(FIXED_WINDOW_SCRIPT),
            fixed_window_refund: Script::new(FIXED_WINDOW_REFUND_SCRIPT),
            token_bucket: Script::new(TOKEN_BUCKET_SCRIPT),
            token_bucket_refund: Script::new(TOKEN_BUCKET_REFUND_SCRIPT),
            sliding_window: Script::new(SLIDING_WINDOW_SCRIPT),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitOperation {
    Consume(u64),
    Probe,
    Refund(u64),
}

#[derive(Debug, Clone, Copy)]
pub struct Decision {
    pub allowed: bool,
    pub observed: i64,
}

impl From<ScriptOutcome> for Decision {
    fn from(outcome: ScriptOutcome) -> Self {
        Self {
            allowed: outcome.allowed,
            observed: outcome.observed,
        }
    }
}

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

struct DescriptorMatch {
    limits_to_check: Vec<(String, RateLimit)>,
    operation: HitOperation,
}

enum DescriptorEvaluation {
    Unmatched,
    Matched(DescriptorMatch),
}

enum PreparedRequest {
    Unconfigured(Vec<DescriptorStatus>),
    Configured(Vec<DescriptorEvaluation>),
}

#[derive(Clone)]
pub struct Steward {
    config_rx: Receiver<RateLimitConfigs>,
    redis: ConnectionManager,
    metrics: SharedMetrics,
    scripts: StewardScripts,
    pub execution_timeout: Duration,
    pub admission_semaphore: Arc<tokio::sync::Semaphore>,
    pub in_flight: Arc<AtomicU64>,
}

/// RAII guard to track and automatically decrement in-flight requests on drop.
pub struct InFlightGuard {
    metrics: SharedMetrics,
    counter: Arc<AtomicU64>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let prev = self.counter.fetch_sub(1, Ordering::SeqCst);
        let in_flight = prev.saturating_sub(1);
        gauge(&self.metrics, "in_flight_requests", in_flight);
    }
}

/// Sanitize connection URLs for safe diagnostic logging, redacting passwords/credentials.
pub fn sanitize_url(raw: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(raw) {
        if parsed.password().is_some() {
            let _ = parsed.set_password(Some("*****"));
        }
        parsed.to_string()
    } else {
        raw.to_string()
    }
}

/// Normalize Redis connection target to a valid redis://, rediss://, or unix:// URL.
pub fn normalize_redis_url(target: &str) -> Result<String, String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err("Redis connection target cannot be empty".to_string());
    }
    let url_str = if trimmed.starts_with("redis://")
        || trimmed.starts_with("rediss://")
        || trimmed.starts_with("unix://")
    {
        trimmed.to_string()
    } else {
        format!("redis://{trimmed}")
    };

    url::Url::parse(&url_str)
        .map_err(|e| format!("invalid Redis URL '{}': {e}", sanitize_url(&url_str)))?;

    Ok(url_str)
}

impl Steward {
    /// Construct a service with metrics disabled.
    pub async fn new(redis_target: &str, config_rx: Receiver<RateLimitConfigs>) -> Self {
        Self::try_new(
            redis_target,
            config_rx,
            std::sync::Arc::new(StatsdClient::from_sink("", NopMetricSink)),
        )
        .await
        .expect("failed to create Steward service")
    }

    /// Construct a service and return configuration or connection manager initialization errors.
    pub async fn try_new(
        redis_target: &str,
        config_rx: Receiver<RateLimitConfigs>,
        metrics: SharedMetrics,
    ) -> Result<Self, String> {
        let redis_url = normalize_redis_url(redis_target)?;
        let client = redis::Client::open(redis_url.as_str()).map_err(|error| {
            format!(
                "invalid Redis configuration ({}): {error}",
                sanitize_url(&redis_url)
            )
        })?;
        let redis = ConnectionManager::new(client).await.map_err(|error| {
            format!(
                "failed to create Redis connection manager ({}): {error}",
                sanitize_url(&redis_url)
            )
        })?;

        Ok(Self {
            config_rx,
            redis,
            metrics,
            scripts: StewardScripts::default(),
            execution_timeout: DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn with_execution_timeout(mut self, timeout: Duration) -> Self {
        self.execution_timeout = timeout;
        self
    }

    pub fn with_max_concurrent_requests(mut self, max_concurrent_requests: usize) -> Self {
        self.admission_semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent_requests));
        self
    }

    pub fn with_admission_semaphore(mut self, semaphore: Arc<tokio::sync::Semaphore>) -> Self {
        self.admission_semaphore = semaphore;
        self
    }

    pub fn with_metrics(mut self, metrics: SharedMetrics) -> Self {
        self.metrics = metrics;
        self
    }

    pub fn in_flight_requests(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    pub fn active_version_num(&self) -> u64 {
        let hash = self.config_rx.borrow().version_hash.clone();
        let prefix = &hash[..16.min(hash.len())];
        u64::from_str_radix(prefix, 16).unwrap_or(0)
    }

    /// Calculate the effective execution timeout given an optional client timeout.
    /// If client deadline is present, `min(client_timeout.saturating_sub(2ms).max(1ms), self.execution_timeout)`;
    /// if not present, use `self.execution_timeout`.
    pub fn effective_timeout(&self, client_timeout: Option<Duration>) -> Duration {
        match client_timeout {
            Some(client_timeout) => {
                let reserve = Duration::from_millis(2);
                let min_timeout = Duration::from_millis(1);
                let client_budget = client_timeout.saturating_sub(reserve).max(min_timeout);
                std::cmp::min(client_budget, self.execution_timeout)
            }
            None => self.execution_timeout,
        }
    }

    pub fn active_config(&self) -> Arc<CompiledConfig> {
        Arc::clone(&*self.config_rx.borrow())
    }

    pub fn active_version_hash(&self) -> String {
        self.config_rx.borrow().version_hash.clone()
    }

    pub fn config_age_seconds(&self) -> u64 {
        self.config_rx.borrow().age_seconds()
    }

    #[cfg(test)]
    pub async fn for_test(config_rx: Receiver<RateLimitConfigs>) -> Self {
        let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
        let redis = match ConnectionManager::new(client.clone()).await {
            Ok(mgr) => mgr,
            Err(_) => ConnectionManager::new_lazy_with_config(
                client,
                redis::aio::ConnectionManagerConfig::default(),
            )
            .unwrap(),
        };
        Self {
            config_rx,
            redis,
            metrics: Arc::new(StatsdClient::from_sink("", NopMetricSink)),
            scripts: StewardScripts::default(),
            execution_timeout: DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn invoke_with_noscript_recovery<'a, T, F>(
        script: &'a Script,
        conn: &mut ConnectionManager,
        build: F,
    ) -> redis::RedisResult<T>
    where
        T: redis::FromRedisValue,
        F: Fn() -> redis::ScriptInvocation<'a>,
    {
        let mut cloned_conn = conn.clone();
        match build().invoke_async(&mut cloned_conn).await {
            Ok(res) => Ok(res),
            Err(err) if err.to_string().contains("NOSCRIPT") => {
                tracing::warn!("NOSCRIPT detected; reloading script into Redis and retrying");
                script.load_async(&mut cloned_conn).await?;
                build().invoke_async(&mut cloned_conn).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn check_limit(
        &self,
        key: &str,
        limit: &RateLimit,
        op: HitOperation,
    ) -> redis::RedisResult<Decision> {
        let mut connection = self.redis.clone();
        let window_seconds = limit.unit.seconds().unwrap_or(60).max(1);

        match op {
            HitOperation::Probe => match limit.algorithm {
                Algorithm::FixedWindow => {
                    let current: Option<i64> = redis::cmd("GET")
                        .arg(key)
                        .query_async(&mut connection)
                        .await?;
                    let current = current.unwrap_or(0);
                    Ok(Decision {
                        allowed: current <= limit.requests_per_unit,
                        observed: current,
                    })
                }
                Algorithm::TokenBucket => {
                    let window_ms = window_seconds.saturating_mul(1_000).max(1);
                    let capacity = limit.requests_per_unit as f64;
                    let refill_per_ms = capacity / window_ms as f64;
                    let state: (Option<f64>, Option<i64>) = redis::cmd("HMGET")
                        .arg(key)
                        .arg("tokens")
                        .arg("timestamp_ms")
                        .query_async(&mut connection)
                        .await?;
                    let tokens = match state.0 {
                        None => capacity,
                        Some(t) => {
                            let redis_time: (i64, i64) =
                                redis::cmd("TIME").query_async(&mut connection).await?;
                            let mut now_ms = (redis_time.0 * 1000) + (redis_time.1 / 1000);
                            let last = state.1.unwrap_or(now_ms);
                            if now_ms < last {
                                now_ms = last;
                            }
                            let elapsed = (now_ms - last).max(0);
                            (t + elapsed as f64 * refill_per_ms).min(capacity)
                        }
                    };
                    let allowed = tokens >= 1.0;
                    Ok(Decision {
                        allowed,
                        observed: tokens.floor() as i64,
                    })
                }
                Algorithm::SlidingWindow => {
                    let window_ms = window_seconds.saturating_mul(1_000).max(1) as i64;
                    let redis_time: (i64, i64) =
                        redis::cmd("TIME").query_async(&mut connection).await?;
                    let now_ms = (redis_time.0 * 1000) + (redis_time.1 / 1000);
                    let count: i64 = redis::cmd("ZCOUNT")
                        .arg(key)
                        .arg(now_ms - window_ms)
                        .arg("+inf")
                        .query_async(&mut connection)
                        .await?;
                    Ok(Decision {
                        allowed: count <= limit.requests_per_unit,
                        observed: count,
                    })
                }
            },
            HitOperation::Refund(hits) => match limit.algorithm {
                Algorithm::FixedWindow => {
                    let current: i64 = Self::invoke_with_noscript_recovery(
                        &self.scripts.fixed_window_refund,
                        &mut connection,
                        || {
                            let mut inv = self.scripts.fixed_window_refund.key(key);
                            inv.arg(hits as i64);
                            inv
                        },
                    )
                    .await?;
                    Ok(Decision {
                        allowed: true,
                        observed: current,
                    })
                }
                Algorithm::TokenBucket => {
                    let window_ms = window_seconds.saturating_mul(1_000).max(1);
                    let capacity = limit.requests_per_unit as f64;
                    let refill_per_ms = capacity / window_ms as f64;
                    let outcome: ScriptOutcome = Self::invoke_with_noscript_recovery(
                        &self.scripts.token_bucket_refund,
                        &mut connection,
                        || {
                            let mut inv = self.scripts.token_bucket_refund.key(key);
                            inv.arg(capacity);
                            inv.arg(refill_per_ms);
                            inv.arg(hits as i64);
                            inv.arg(window_ms);
                            inv
                        },
                    )
                    .await?;
                    Ok(outcome.into())
                }
                Algorithm::SlidingWindow => Err(redis::RedisError::from((
                    redis::ErrorKind::Client,
                    "refunds are unsupported for sliding window",
                ))),
            },
            HitOperation::Consume(hits) => {
                let hits = hits as i64;
                match limit.algorithm {
                    Algorithm::FixedWindow => {
                        let current: i64 = Self::invoke_with_noscript_recovery(
                            &self.scripts.fixed_window,
                            &mut connection,
                            || {
                                let mut inv = self.scripts.fixed_window.key(key);
                                inv.arg(hits);
                                inv.arg(window_seconds);
                                inv
                            },
                        )
                        .await?;
                        Ok(Decision {
                            allowed: current <= limit.requests_per_unit,
                            observed: current,
                        })
                    }
                    Algorithm::TokenBucket => {
                        let window_ms = window_seconds.saturating_mul(1_000).max(1);
                        let capacity = limit.requests_per_unit as f64;
                        let refill_per_ms = capacity / window_ms as f64;
                        let outcome: ScriptOutcome = Self::invoke_with_noscript_recovery(
                            &self.scripts.token_bucket,
                            &mut connection,
                            || {
                                let mut inv = self.scripts.token_bucket.key(key);
                                inv.arg(capacity);
                                inv.arg(refill_per_ms);
                                inv.arg(hits);
                                inv.arg(window_ms);
                                inv
                            },
                        )
                        .await?;
                        Ok(outcome.into())
                    }
                    Algorithm::SlidingWindow => {
                        if hits > 100 {
                            return Err(redis::RedisError::from((
                                redis::ErrorKind::Client,
                                "hit cost exceeds maximum of 100 for sliding window",
                            )));
                        }
                        let window_ms = window_seconds.saturating_mul(1_000).max(1);
                        let nonce = generate_sliding_window_nonce();
                        let outcome: ScriptOutcome = Self::invoke_with_noscript_recovery(
                            &self.scripts.sliding_window,
                            &mut connection,
                            || {
                                let mut inv = self.scripts.sliding_window.key(key);
                                inv.arg(window_ms);
                                inv.arg(limit.requests_per_unit);
                                inv.arg(hits);
                                inv.arg(&nonce);
                                inv
                            },
                        )
                        .await?;
                        Ok(outcome.into())
                    }
                }
            }
        }
    }

    pub async fn execute_check_limit(
        &self,
        key: &str,
        limit: &RateLimit,
        op: HitOperation,
    ) -> Result<Decision, redis::RedisError> {
        let started = std::time::Instant::now();
        let res = self.check_limit(key, limit, op).await;
        time(&self.metrics, "redis.operation_time", started.elapsed());
        match &res {
            Ok(_decision) => {
                // Ambiguous fleet gauge `rate_limit.observed` eliminated per F13
            }
            Err(error) => {
                static REDIS_ERROR_LIMITER: ErrorRateLimiter = ErrorRateLimiter::new(1000);
                if let Some(suppressed) = REDIS_ERROR_LIMITER.check() {
                    if suppressed > 0 {
                        error!(
                            algorithm = ?limit.algorithm,
                            unit = ?limit.unit,
                            %error,
                            suppressed_errors = suppressed,
                            "failed to update rate limit in Redis (some errors suppressed)"
                        );
                    } else {
                        error!(
                            algorithm = ?limit.algorithm,
                            unit = ?limit.unit,
                            %error,
                            "failed to update rate limit in Redis"
                        );
                    }
                }
                if is_redis_timeout(error) {
                    count(&self.metrics, "redis.timeouts", 1);
                } else {
                    count(&self.metrics, "redis.errors", 1);
                }
            }
        }
        res
    }
}

pub fn is_redis_timeout(err: &redis::RedisError) -> bool {
    if err.is_timeout() {
        return true;
    }
    let desc = err.to_string().to_lowercase();
    if desc.contains("failed to acquire redis connection") {
        return false;
    }
    if let Some(detail) = err.detail() {
        let lower = detail.to_lowercase();
        if lower.contains("timeout") || lower.contains("timed out") || lower.contains("deadline") {
            return true;
        }
    }
    desc.contains("timeout") || desc.contains("timed out") || desc.contains("deadline")
}

pub fn duration_until_reset_for(limit: &RateLimit) -> u64 {
    let window_secs = limit.unit.seconds().unwrap_or(60).max(1);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rem = now % window_secs;
    if rem == 0 {
        window_secs
    } else {
        window_secs - rem
    }
}

pub fn limit_remaining_for(limit: &RateLimit, decision: &Decision) -> u32 {
    match limit.algorithm {
        Algorithm::FixedWindow | Algorithm::SlidingWindow => limit
            .requests_per_unit
            .saturating_sub(decision.observed)
            .max(0) as u32,
        Algorithm::TokenBucket => decision.observed.max(0) as u32,
    }
}

pub fn aggregate_descriptor_status(
    limit_decisions: &[(&RateLimit, &Decision)],
) -> DescriptorStatus {
    if limit_decisions.is_empty() {
        return DescriptorStatus {
            code: Code::Ok as i32,
            current_limit: None,
            limit_remaining: 0,
            duration_until_reset: None,
            quota: None,
        };
    }

    let any_over = limit_decisions.iter().any(|(_, dec)| !dec.allowed);

    if any_over {
        // Section 5.2 Rule 2: Over-Limit Status Governing Rule
        // Select violated window with largest duration_until_reset (tie-breaker: shortest unit duration).
        let mut violated: Vec<(&RateLimit, &Decision)> = limit_decisions
            .iter()
            .copied()
            .filter(|(_, dec)| !dec.allowed)
            .collect();

        violated.sort_by(|(l1, _), (l2, _)| {
            let reset1 = duration_until_reset_for(l1);
            let reset2 = duration_until_reset_for(l2);
            let unit_secs1 = l1.unit.seconds().unwrap_or(60);
            let unit_secs2 = l2.unit.seconds().unwrap_or(60);

            reset2
                .cmp(&reset1)
                .then_with(|| unit_secs1.cmp(&unit_secs2))
        });

        let (gov_limit, gov_decision) = violated[0];
        let remaining = limit_remaining_for(gov_limit, gov_decision);
        let reset_secs = duration_until_reset_for(gov_limit);

        DescriptorStatus {
            code: Code::OverLimit as i32,
            current_limit: Some(gov_limit.to_proto()),
            limit_remaining: remaining,
            duration_until_reset: Some(prost_types::Duration {
                seconds: reset_secs as i64,
                nanos: 0,
            }),
            quota: None,
        }
    } else {
        // Section 5.2 Rule 3: Allowed Status Governing Rule
        // Select window with lowest ratio of remaining capacity: limit_remaining / capacity
        // Tie-breakers: lowest absolute limit_remaining, then shortest unit duration.
        let mut allowed: Vec<(&RateLimit, &Decision)> = limit_decisions.to_vec();

        allowed.sort_by(|(l1, d1), (l2, d2)| {
            let rem1 = limit_remaining_for(l1, d1);
            let rem2 = limit_remaining_for(l2, d2);
            let cap1 = l1.requests_per_unit.max(1) as u64;
            let cap2 = l2.requests_per_unit.max(1) as u64;

            let ratio_cmp = (rem1 as u128 * cap2 as u128).cmp(&(rem2 as u128 * cap1 as u128));
            let unit_secs1 = l1.unit.seconds().unwrap_or(60);
            let unit_secs2 = l2.unit.seconds().unwrap_or(60);

            ratio_cmp
                .then_with(|| rem1.cmp(&rem2))
                .then_with(|| unit_secs1.cmp(&unit_secs2))
        });

        let (gov_limit, gov_decision) = allowed[0];
        let remaining = limit_remaining_for(gov_limit, gov_decision);
        let reset_secs = duration_until_reset_for(gov_limit);

        DescriptorStatus {
            code: Code::Ok as i32,
            current_limit: Some(gov_limit.to_proto()),
            limit_remaining: remaining,
            duration_until_reset: Some(prost_types::Duration {
                seconds: reset_secs as i64,
                nanos: 0,
            }),
            quota: None,
        }
    }
}

pub fn validate_request(request: &RateLimitRequest) -> Result<(), tonic::Status> {
    if request.domain.is_empty() {
        return Err(tonic::Status::invalid_argument("domain cannot be empty"));
    }
    if request.domain.len() > 128 {
        return Err(tonic::Status::invalid_argument(
            "domain length exceeds maximum of 128 bytes",
        ));
    }
    if request.descriptors.len() > 16 {
        return Err(tonic::Status::invalid_argument(
            "descriptors count exceeds maximum of 16",
        ));
    }
    if request.hits_addend > 100 {
        return Err(tonic::Status::invalid_argument(
            "request hits_addend exceeds maximum of 100",
        ));
    }

    for desc in &request.descriptors {
        if desc.entries.is_empty() {
            return Err(tonic::Status::invalid_argument(
                "descriptor must have at least 1 entry",
            ));
        }
        if desc.entries.len() > 8 {
            return Err(tonic::Status::invalid_argument(
                "descriptor entries count exceeds maximum of 8",
            ));
        }
        for entry in &desc.entries {
            if entry.key.is_empty() {
                return Err(tonic::Status::invalid_argument(
                    "descriptor entry key cannot be empty",
                ));
            }
            if entry.key.len() > 256 {
                return Err(tonic::Status::invalid_argument(
                    "descriptor entry key length exceeds maximum of 256 bytes",
                ));
            }
            if entry.value.len() > 256 {
                return Err(tonic::Status::invalid_argument(
                    "descriptor entry value length exceeds maximum of 256 bytes",
                ));
            }
        }
        if let Some(hits) = desc.hits_addend
            && hits > 100
        {
            return Err(tonic::Status::invalid_argument(format!(
                "descriptor hits_addend ({hits}) exceeds maximum of 100"
            )));
        }
        if let Some(ref override_) = desc.limit
            && let Err(msg) = crate::rate_limits::validate_override(override_)
        {
            return Err(tonic::Status::invalid_argument(msg));
        }
    }

    Ok(())
}

pub fn compute_descriptor_hit_cost(
    desc: &crate::proto::envoy::extensions::common::ratelimit::v3::RateLimitDescriptor,
    request_hits_addend: u32,
) -> Result<u64, tonic::Status> {
    let cost = match desc.hits_addend {
        Some(val) => val,
        None if request_hits_addend > 0 => request_hits_addend as u64,
        None => 1,
    };
    if cost > 100 {
        return Err(tonic::Status::invalid_argument(format!(
            "hit cost ({cost}) exceeds maximum of 100"
        )));
    }
    Ok(cost)
}

impl Steward {
    fn prepare_request(
        &self,
        request: &RateLimitRequest,
        rpc_start: std::time::Instant,
    ) -> Result<PreparedRequest, tonic::Status> {
        validate_request(request).map_err(|status| {
            count(&self.metrics, "requests.invalid", 1);
            time(&self.metrics, "rpc.duration", rpc_start.elapsed());
            status
        })?;

        let configs = self.config_rx.borrow();
        let Some(domain_policy) = configs.get(&request.domain) else {
            count(&self.metrics, "requests.unconfigured", 1);
            count(&self.metrics, "requests.allowed", 1);
            time(&self.metrics, "rpc.duration.allowed", rpc_start.elapsed());
            time(&self.metrics, "rpc.duration", rpc_start.elapsed());
            debug!(domain = %request.domain, "unconfigured domain allowed");
            let statuses = request
                .descriptors
                .iter()
                .map(|_| {
                    count(&self.metrics, "descriptors.unmatched", 1);
                    DescriptorStatus {
                        code: Code::Ok as i32,
                        current_limit: None,
                        limit_remaining: 0,
                        duration_until_reset: None,
                        quota: None,
                    }
                })
                .collect();
            return Ok(PreparedRequest::Unconfigured(statuses));
        };

        let mut evaluations = Vec::with_capacity(request.descriptors.len());
        for req_desc in &request.descriptors {
            let hit_cost = compute_descriptor_hit_cost(req_desc, request.hits_addend)?;
            let operation = if req_desc.is_negative_hits {
                HitOperation::Refund(hit_cost)
            } else if hit_cost == 0 {
                HitOperation::Probe
            } else {
                HitOperation::Consume(hit_cost)
            };

            let entries: Vec<(&str, &str)> = req_desc
                .entries
                .iter()
                .map(|entry| (entry.key.as_str(), entry.value.as_str()))
                .collect();
            let Some(matched) = domain_policy.match_entries(&entries) else {
                evaluations.push(DescriptorEvaluation::Unmatched);
                continue;
            };

            if req_desc.is_negative_hits
                && matched
                    .rate_limits
                    .iter()
                    .any(|limit| limit.algorithm == Algorithm::SlidingWindow)
            {
                time(&self.metrics, "rpc.duration", rpc_start.elapsed());
                return Err(tonic::Status::failed_precondition(
                    "refunds are unsupported for sliding-window rate limits",
                ));
            }

            let encoded_path = encode_canonical_path(entries);
            let mut limits_to_check = Vec::with_capacity(matched.rate_limits.len());
            for configured_limit in matched.rate_limits {
                let effective_limit = req_desc
                    .limit
                    .as_ref()
                    .map(|override_| configured_limit.with_override(override_))
                    .unwrap_or_else(|| *configured_limit);
                let key = rate_limit_key(
                    &request.domain,
                    matched.policy_id,
                    &encoded_path,
                    configured_limit,
                );
                limits_to_check.push((key, effective_limit));
            }

            if limits_to_check.is_empty() {
                evaluations.push(DescriptorEvaluation::Unmatched);
            } else {
                evaluations.push(DescriptorEvaluation::Matched(DescriptorMatch {
                    limits_to_check,
                    operation,
                }));
            }
        }
        Ok(PreparedRequest::Configured(evaluations))
    }

    async fn handle_request(
        &self,
        request: tonic::Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, tonic::Status> {
        let rpc_start = std::time::Instant::now();
        count(&self.metrics, "requests.total", 1);

        // Global Admission Control (Load Shedding): Immediate non-blocking permit acquisition
        let admission_start = std::time::Instant::now();
        let _permit = match self.admission_semaphore.clone().try_acquire_owned() {
            Ok(permit) => {
                time(
                    &self.metrics,
                    "admission.wait_time",
                    admission_start.elapsed(),
                );
                permit
            }
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                time(
                    &self.metrics,
                    "admission.wait_time",
                    admission_start.elapsed(),
                );
                time(&self.metrics, "rpc.duration", rpc_start.elapsed());
                count(&self.metrics, "requests.rejected_admission", 1);
                return Err(tonic::Status::resource_exhausted(
                    "admission limit reached; request rejected due to load shedding",
                ));
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                time(
                    &self.metrics,
                    "admission.wait_time",
                    admission_start.elapsed(),
                );
                time(&self.metrics, "rpc.duration", rpc_start.elapsed());
                return Err(tonic::Status::unavailable("service is shutting down"));
            }
        };

        // Track in-flight requests with RAII guard
        let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        gauge(&self.metrics, "in_flight_requests", in_flight);
        let _in_flight_guard = InFlightGuard {
            metrics: self.metrics.clone(),
            counter: self.in_flight.clone(),
        };

        // Record active config age and version
        gauge(
            &self.metrics,
            "config.age_seconds",
            self.config_age_seconds(),
        );
        gauge(&self.metrics, "config.version", self.active_version_num());

        let (metadata, _, request) = request.into_parts();

        let evaluations = match self.prepare_request(&request, rpc_start)? {
            PreparedRequest::Unconfigured(statuses) => {
                return Ok(Response::new(build_response(false, statuses)));
            }
            PreparedRequest::Configured(evaluations) => evaluations,
        };

        debug!(
            domain = %request.domain,
            descriptors = request.descriptors.len(),
            version_hash = %self.config_rx.borrow().version_hash,
            "evaluating rate limits"
        );

        // Effective timeout calculation from gRPC deadline header
        let client_timeout = metadata
            .get("grpc-timeout")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_grpc_timeout);
        let effective_timeout = self.effective_timeout(client_timeout);

        // 4. Redis Evaluation Phase (wrapped in bounded timeout, preserving 1:1 input descriptor order)
        let redis_start = std::time::Instant::now();
        let eval_result = tokio::time::timeout(effective_timeout, async {
            let eval_futures = evaluations.into_iter().map(|eval| async move {
                match eval {
                    DescriptorEvaluation::Unmatched => {
                        count(&self.metrics, "descriptors.unmatched", 1);
                        let status = DescriptorStatus {
                            code: Code::Ok as i32,
                            current_limit: None,
                            limit_remaining: 0,
                            duration_until_reset: None,
                            quota: None,
                        };
                        (status, false, None)
                    }
                    DescriptorEvaluation::Matched(desc_match) => {
                        let rule_futures = desc_match.limits_to_check.iter().map(|(key, limit)| {
                            let op = desc_match.operation;
                            async move {
                                let result = self.execute_check_limit(key, limit, op).await;
                                (limit, result)
                            }
                        });
                        let rule_results = futures_util::future::join_all(rule_futures).await;

                        let mut successful_decisions = Vec::with_capacity(rule_results.len());
                        let mut failed_rules = Vec::new();
                        let mut desc_over_limit = false;
                        let mut desc_redis_error = None;

                        for (limit, res) in rule_results {
                            match res {
                                Ok(decision) => {
                                    if !decision.allowed {
                                        desc_over_limit = true;
                                    }
                                    successful_decisions.push((limit, decision));
                                }
                                Err(error) => {
                                    if desc_redis_error.is_none() {
                                        desc_redis_error = Some(error.clone());
                                    }
                                    failed_rules.push((limit, error));
                                }
                            }
                        }

                        let has_over_limit =
                            successful_decisions.iter().any(|(_, dec)| !dec.allowed);

                        let status = if has_over_limit {
                            let violated: Vec<(&RateLimit, &Decision)> = successful_decisions
                                .iter()
                                .filter(|(_, dec)| !dec.allowed)
                                .map(|(l, d)| (*l, d))
                                .collect();
                            aggregate_descriptor_status(&violated)
                        } else if failed_rules.is_empty() {
                            let pairs: Vec<(&RateLimit, &Decision)> =
                                successful_decisions.iter().map(|(l, d)| (*l, d)).collect();
                            aggregate_descriptor_status(&pairs)
                        } else {
                            let gov_limit = successful_decisions
                                .first()
                                .map(|(l, _)| **l)
                                .unwrap_or(desc_match.limits_to_check[0].1);
                            let reset_secs = duration_until_reset_for(&gov_limit);
                            DescriptorStatus {
                                code: Code::Unknown as i32,
                                current_limit: Some(gov_limit.to_proto()),
                                limit_remaining: 0,
                                duration_until_reset: Some(prost_types::Duration {
                                    seconds: reset_secs as i64,
                                    nanos: 0,
                                }),
                                quota: None,
                            }
                        };

                        (status, desc_over_limit, desc_redis_error)
                    }
                }
            });

            let descriptor_results = futures_util::future::join_all(eval_futures).await;

            let mut statuses = Vec::with_capacity(descriptor_results.len());
            let mut any_rule_over_limit = false;
            let mut first_redis_error = None;

            for (status, over_limit, redis_err) in descriptor_results {
                statuses.push(status);
                if over_limit {
                    any_rule_over_limit = true;
                }
                if first_redis_error.is_none() && redis_err.is_some() {
                    first_redis_error = redis_err;
                }
            }

            (statuses, any_rule_over_limit, first_redis_error)
        })
        .await;
        time(&self.metrics, "redis.duration", redis_start.elapsed());

        let (statuses, any_rule_over_limit, first_redis_error) = match eval_result {
            Ok(result) => result,
            Err(_elapsed) => {
                count(&self.metrics, "redis.timeouts", 1);
                count(&self.metrics, "requests.deadline_exceeded", 1);
                time(&self.metrics, "rpc.duration", rpc_start.elapsed());
                return Err(tonic::Status::deadline_exceeded(
                    "request execution deadline exceeded",
                ));
            }
        };

        // 5. Overall response code aggregation & F06 Error Precedence Rule
        if any_rule_over_limit {
            // Rule 1: Definitive quota rejection WINS!
            count(&self.metrics, "requests.over_limit", 1);
            time(&self.metrics, "rpc.duration.denied", rpc_start.elapsed());
            time(&self.metrics, "rpc.duration", rpc_start.elapsed());
            debug!(domain = %request.domain, "request is over the rate limit");
            debug!(
                domain = %request.domain,
                over_limit = true,
                "rate limit decision complete"
            );
            Ok(Response::new(build_response(true, statuses)))
        } else if let Some(err) = first_redis_error {
            // Rule 2: If NO rule was over limit, but one or more backend operations failed with a Redis error
            time(&self.metrics, "rpc.duration", rpc_start.elapsed());
            if is_redis_timeout(&err) {
                count(&self.metrics, "requests.deadline_exceeded", 1);
                Err(tonic::Status::deadline_exceeded(
                    "rate limit storage backend request timed out",
                ))
            } else {
                Err(tonic::Status::unavailable(
                    "rate limit storage backend is unavailable",
                ))
            }
        } else {
            count(&self.metrics, "requests.allowed", 1);
            time(&self.metrics, "rpc.duration.allowed", rpc_start.elapsed());
            time(&self.metrics, "rpc.duration", rpc_start.elapsed());
            debug!(
                domain = %request.domain,
                over_limit = false,
                "rate limit decision complete"
            );
            Ok(Response::new(build_response(false, statuses)))
        }
    }
}

#[tonic::async_trait]
impl RateLimitService for Steward {
    async fn should_rate_limit(
        &self,
        request: tonic::Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, tonic::Status> {
        self.handle_request(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::{aggregate_descriptor_status, duration_until_reset_for, limit_remaining_for};
    use crate::proto::envoy::service::ratelimit::v3::rate_limit_response::Code;
    use crate::rate_limits::{
        Algorithm, DescriptorConfig, PolicyTrie, RateLimit, Unit, encode_canonical_path,
        rate_limit_key,
    };
    use crate::service::Decision;

    #[test]
    fn descriptor_keys_do_not_collide_when_values_share_prefixes() {
        let first_path = encode_canonical_path([("a", "bc")]);
        let second_path = encode_canonical_path([("ab", "c")]);
        let limit = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 1,
        };

        assert_ne!(
            rate_limit_key("domain", "default", &first_path, &limit),
            rate_limit_key("domain", "default", &second_path, &limit)
        );
    }

    #[test]
    fn unmatched_descriptor_returns_unconstrained_status() {
        let rule = DescriptorConfig {
            key: "known".to_string(),
            value: Some("1".to_string()),
            rate_limit: Some(RateLimit {
                algorithm: Algorithm::FixedWindow,
                unit: Unit::Seconds,
                requests_per_unit: 10,
            }),
            rate_limits: None,
            descriptors: None,
            id: None,
            policy_id: None,
        };
        let trie = PolicyTrie::from_descriptors(&[rule]);

        let matched = trie.match_entries(&[("unknown", "value")]);
        assert!(matched.is_none());
    }

    #[test]
    fn multi_window_status_aggregation_allowed() {
        // Limit 1: 10/s, observed 3 -> remaining 7, ratio 7/10 = 0.7
        // Limit 2: 100/min, observed 80 -> remaining 20, ratio 20/100 = 0.2 (governing)
        let limit_sec = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 10,
        };
        let dec_sec = Decision {
            allowed: true,
            observed: 3,
        };

        let limit_min = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Minutes,
            requests_per_unit: 100,
        };
        let dec_min = Decision {
            allowed: true,
            observed: 80,
        };

        let status = aggregate_descriptor_status(&[(&limit_sec, &dec_sec), (&limit_min, &dec_min)]);

        assert_eq!(status.code, Code::Ok as i32);
        // Lowest remaining ratio (0.2) is the minute limit
        assert_eq!(status.limit_remaining, 20);
        assert_eq!(status.current_limit.unwrap().requests_per_unit, 100);
    }

    #[test]
    fn multi_window_status_aggregation_over_limit() {
        // Limit 1: 10/s, observed 15 -> OVER_LIMIT, duration_until_reset = 1s
        // Limit 2: 100/min, observed 120 -> OVER_LIMIT, duration_until_reset = 60s (governing, largest reset)
        let limit_sec = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 10,
        };
        let dec_sec = Decision {
            allowed: false,
            observed: 15,
        };

        let limit_min = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Minutes,
            requests_per_unit: 100,
        };
        let dec_min = Decision {
            allowed: false,
            observed: 120,
        };

        let status = aggregate_descriptor_status(&[(&limit_sec, &dec_sec), (&limit_min, &dec_min)]);

        assert_eq!(status.code, Code::OverLimit as i32);
        // Governing violated window is the minute window (longest reset)
        assert_eq!(status.current_limit.unwrap().requests_per_unit, 100);
        assert_eq!(status.limit_remaining, 0);
    }

    #[test]
    fn test_limit_remaining_calculation() {
        let fw_limit = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 10,
        };
        assert_eq!(
            limit_remaining_for(
                &fw_limit,
                &Decision {
                    allowed: true,
                    observed: 4
                }
            ),
            6
        );
        assert_eq!(
            limit_remaining_for(
                &fw_limit,
                &Decision {
                    allowed: false,
                    observed: 15
                }
            ),
            0
        );

        let tb_limit = RateLimit {
            algorithm: Algorithm::TokenBucket,
            unit: Unit::Seconds,
            requests_per_unit: 10,
        };
        assert_eq!(
            limit_remaining_for(
                &tb_limit,
                &Decision {
                    allowed: true,
                    observed: 7
                }
            ),
            7
        );
    }

    #[test]
    fn test_duration_until_reset_bounds() {
        let limit = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Minutes,
            requests_per_unit: 10,
        };
        let reset = duration_until_reset_for(&limit);
        assert!(reset > 0);
        assert!(reset <= 60);
    }

    #[tokio::test]
    async fn steward_exposes_active_version_hash_and_age() {
        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let expected_hash = compiled.version_hash.clone();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);

        let steward = super::Steward::for_test(rx).await;
        assert_eq!(steward.active_version_hash(), expected_hash);
        assert_eq!(steward.active_config().version_hash, expected_hash);
        let _ = steward.config_age_seconds();
    }

    #[test]
    fn request_validation_rejects_overbound_dimensions_and_malformed_inputs() {
        use super::validate_request;
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor,
            rate_limit_descriptor::{Entry, RateLimitOverride},
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use tonic::Code;

        let valid_desc = RateLimitDescriptor {
            entries: vec![Entry {
                key: "k".to_string(),
                value: "v".to_string(),
            }],
            limit: None,
            hits_addend: None,
            is_negative_hits: false,
        };

        // 1. Empty domain
        let mut req = RateLimitRequest {
            domain: "".to_string(),
            descriptors: vec![valid_desc.clone()],
            hits_addend: 1,
        };
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // 2. Oversized domain (> 128 bytes)
        req.domain = "a".repeat(129);
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // Domain at bound (128 bytes) passes
        req.domain = "a".repeat(128);
        assert!(validate_request(&req).is_ok());

        // 3. Descriptors count > 16
        req.descriptors = vec![valid_desc.clone(); 17];
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // Descriptors count at bound (16) passes
        req.descriptors = vec![valid_desc.clone(); 16];
        assert!(validate_request(&req).is_ok());

        // 4. Descriptor with empty entries
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![],
            limit: None,
            hits_addend: None,
            is_negative_hits: false,
        }];
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // 5. Descriptor with entries count > 8
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![
                Entry {
                    key: "k".to_string(),
                    value: "v".to_string()
                };
                9
            ],
            limit: None,
            hits_addend: None,
            is_negative_hits: false,
        }];
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // Entries count at bound (8) passes
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![
                Entry {
                    key: "k".to_string(),
                    value: "v".to_string()
                };
                8
            ],
            limit: None,
            hits_addend: None,
            is_negative_hits: false,
        }];
        assert!(validate_request(&req).is_ok());

        // 6. Entry key empty
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![Entry {
                key: "".to_string(),
                value: "v".to_string(),
            }],
            limit: None,
            hits_addend: None,
            is_negative_hits: false,
        }];
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // 7. Entry key > 256 bytes
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![Entry {
                key: "k".repeat(257),
                value: "v".to_string(),
            }],
            limit: None,
            hits_addend: None,
            is_negative_hits: false,
        }];
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // Entry key at bound (256 bytes) passes
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![Entry {
                key: "k".repeat(256),
                value: "v".to_string(),
            }],
            limit: None,
            hits_addend: None,
            is_negative_hits: false,
        }];
        assert!(validate_request(&req).is_ok());

        // 8. Entry value > 256 bytes
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![Entry {
                key: "k".to_string(),
                value: "v".repeat(257),
            }],
            limit: None,
            hits_addend: None,
            is_negative_hits: false,
        }];
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // Entry value at bound (256 bytes) passes
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![Entry {
                key: "k".to_string(),
                value: "v".repeat(256),
            }],
            limit: None,
            hits_addend: None,
            is_negative_hits: false,
        }];
        assert!(validate_request(&req).is_ok());

        // 9. Request-level hits_addend > 100
        req.descriptors = vec![valid_desc.clone()];
        req.hits_addend = 101;
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        req.hits_addend = 100;
        assert!(validate_request(&req).is_ok());

        // 10. Descriptor-level hits_addend > 100
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![Entry {
                key: "k".to_string(),
                value: "v".to_string(),
            }],
            limit: None,
            hits_addend: Some(101),
            is_negative_hits: false,
        }];
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        req.descriptors[0].hits_addend = Some(100);
        assert!(validate_request(&req).is_ok());

        // 11. Malformed override: requests_per_unit == 0
        req.descriptors = vec![RateLimitDescriptor {
            entries: vec![Entry {
                key: "k".to_string(),
                value: "v".to_string(),
            }],
            limit: Some(RateLimitOverride {
                requests_per_unit: 0,
                unit: 1,
            }),
            hits_addend: None,
            is_negative_hits: false,
        }];
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // 12. Malformed override: unit == Unknown (0)
        req.descriptors[0].limit = Some(RateLimitOverride {
            requests_per_unit: 10,
            unit: 0,
        });
        assert_eq!(
            validate_request(&req).unwrap_err().code(),
            Code::InvalidArgument
        );

        // 13. Valid override
        req.descriptors[0].limit = Some(RateLimitOverride {
            requests_per_unit: 10,
            unit: 1,
        });
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn descriptor_hits_addend_takes_precedence_over_request_level() {
        use super::compute_descriptor_hit_cost;
        use crate::proto::envoy::extensions::common::ratelimit::v3::RateLimitDescriptor;

        let mut desc = RateLimitDescriptor {
            entries: vec![],
            limit: None,
            hits_addend: Some(5),
            is_negative_hits: false,
        };

        // Descriptor hits_addend (5) overrides request hits_addend (10)
        assert_eq!(compute_descriptor_hit_cost(&desc, 10).unwrap(), 5);

        // Zero-hit probe at descriptor level (Some(0)) overrides request hits_addend (10)
        desc.hits_addend = Some(0);
        assert_eq!(compute_descriptor_hit_cost(&desc, 10).unwrap(), 0);

        // When descriptor hits_addend is None, falls back to request hits_addend (if > 0)
        desc.hits_addend = None;
        assert_eq!(compute_descriptor_hit_cost(&desc, 10).unwrap(), 10);

        // When both are absent / 0, defaults to 1
        assert_eq!(compute_descriptor_hit_cost(&desc, 0).unwrap(), 1);

        // Cost > 100 rejected
        desc.hits_addend = Some(101);
        assert!(compute_descriptor_hit_cost(&desc, 0).is_err());
    }

    #[tokio::test]
    async fn sliding_window_refunds_are_rejected() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;
        use tonic::Code;

        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "type",
                    "value": "sliding",
                    "rate_limit": { "algorithm": "sliding_window", "unit": "seconds", "requests_per_unit": 10 }
                },
                {
                    "key": "type",
                    "value": "fixed",
                    "rate_limit": { "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward::for_test(rx).await;

        // Sliding-window state cannot safely remove individual prior hits.
        let req_sliding = RateLimitRequest {
            domain: "default".to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "type".to_string(),
                    value: "sliding".to_string(),
                }],
                limit: None,
                hits_addend: Some(2),
                is_negative_hits: true,
            }],
            hits_addend: 0,
        };
        let res = steward
            .should_rate_limit(tonic::Request::new(req_sliding))
            .await;
        assert_eq!(res.unwrap_err().code(), Code::FailedPrecondition);
    }

    struct TestRedisServer {
        port: u16,
        child: Option<std::process::Child>,
    }

    impl TestRedisServer {
        fn start() -> Option<Self> {
            static NEXT_TEST_PORT: std::sync::atomic::AtomicU16 =
                std::sync::atomic::AtomicU16::new(16500);

            // Check default port 6379 first
            if let Ok(client) = redis::Client::open("redis://127.0.0.1:6379")
                && let Ok(mut conn) = client.get_connection()
                && redis::cmd("PING").query::<String>(&mut conn).is_ok()
            {
                return Some(Self {
                    port: 6379,
                    child: None,
                });
            }

            // Spawn ephemeral redis-server on unique port
            let port = NEXT_TEST_PORT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let child = std::process::Command::new("redis-server")
                .arg("--port")
                .arg(port.to_string())
                .arg("--save")
                .arg("")
                .arg("--appendonly")
                .arg("no")
                .arg("--dir")
                .arg("/tmp")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;

            std::thread::sleep(std::time::Duration::from_millis(250));
            Some(Self {
                port,
                child: Some(child),
            })
        }
    }

    impl Drop for TestRedisServer {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn spawn_dedicated_ephemeral_redis_on_port(port: u16) -> Option<std::process::Child> {
        let child = std::process::Command::new("redis-server")
            .arg("--port")
            .arg(port.to_string())
            .arg("--save")
            .arg("")
            .arg("--appendonly")
            .arg("no")
            .arg("--dir")
            .arg("/tmp")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        std::thread::sleep(std::time::Duration::from_millis(250));
        Some(child)
    }

    fn spawn_dedicated_ephemeral_redis() -> Option<(u16, std::process::Child)> {
        static DEDICATED_PORT: std::sync::atomic::AtomicU16 =
            std::sync::atomic::AtomicU16::new(17500);
        let port = DEDICATED_PORT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let child = spawn_dedicated_ephemeral_redis_on_port(port)?;
        Some((port, child))
    }

    async fn setup_test_steward(
        json_str: &str,
    ) -> Option<(TestRedisServer, super::Steward, redis::Client)> {
        let server = TestRedisServer::start()?;
        let client = redis::Client::open(format!("redis://127.0.0.1:{}", server.port)).ok()?;
        let redis = redis::aio::ConnectionManager::new(client.clone())
            .await
            .ok()?;
        let raw: crate::config_source::RawRateLimitsConfig = serde_json::from_str(json_str).ok()?;
        let compiled = crate::config_source::compile_rate_limits(raw).ok()?;
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: std::time::Duration::from_secs(5),
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        Some((server, steward, client))
    }

    #[tokio::test]
    async fn zero_cost_probe_returns_remaining_without_mutating_redis() {
        let Some(_server) = TestRedisServer::start() else {
            eprintln!("Skipping zero-cost probe test: redis-server not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{}", _server.port)).unwrap();
        let redis = redis::aio::ConnectionManager::new(client.clone())
            .await
            .unwrap();
        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let mut conn = client.get_connection().unwrap();
        let test_key = "test_probe_key";

        // Case 1: Key does not exist in Redis
        let _: () = redis::cmd("DEL").arg(test_key).query(&mut conn).unwrap();
        let limit = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 10,
        };

        let decision = steward
            .check_limit(test_key, &limit, super::HitOperation::Probe)
            .await
            .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.observed, 0);
        assert_eq!(super::limit_remaining_for(&limit, &decision), 10);

        // Verify key was NOT created in Redis
        let exists: bool = redis::cmd("EXISTS").arg(test_key).query(&mut conn).unwrap();
        assert!(!exists, "Probe must not create key in Redis");

        // Case 2: Key exists with count 4
        let _: () = redis::cmd("SET")
            .arg(test_key)
            .arg(4)
            .query(&mut conn)
            .unwrap();
        let decision = steward
            .check_limit(test_key, &limit, super::HitOperation::Probe)
            .await
            .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.observed, 4);
        assert_eq!(super::limit_remaining_for(&limit, &decision), 6);

        // Verify key value in Redis is STILL 4 (not incremented)
        let val: i64 = redis::cmd("GET").arg(test_key).query(&mut conn).unwrap();
        assert_eq!(val, 4, "Probe must not increment or mutate Redis counter");

        // Clean up
        let _: () = redis::cmd("DEL").arg(test_key).query(&mut conn).unwrap();
    }

    #[tokio::test]
    async fn fixed_window_refund_decrements_and_clamps_at_zero() {
        let Some(_server) = TestRedisServer::start() else {
            eprintln!("Skipping fixed-window refund test: redis-server not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{}", _server.port)).unwrap();
        let redis = redis::aio::ConnectionManager::new(client.clone())
            .await
            .unwrap();
        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let mut conn = client.get_connection().unwrap();
        let test_key = "test_refund_key";
        let limit = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 10,
        };

        // Seed counter at 5
        let _: () = redis::cmd("SET")
            .arg(test_key)
            .arg(5)
            .query(&mut conn)
            .unwrap();

        // Refund 2 -> counter should become 3
        let decision = steward
            .check_limit(test_key, &limit, super::HitOperation::Refund(2))
            .await
            .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.observed, 3);
        let val: i64 = redis::cmd("GET").arg(test_key).query(&mut conn).unwrap();
        assert_eq!(val, 3);

        // Refund 5 from counter at 3 -> clamped at 0 (never negative)
        let decision = steward
            .check_limit(test_key, &limit, super::HitOperation::Refund(5))
            .await
            .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.observed, 0);
        let val: i64 = redis::cmd("GET").arg(test_key).query(&mut conn).unwrap();
        assert_eq!(val, 0);

        // Clean up
        let _: () = redis::cmd("DEL").arg(test_key).query(&mut conn).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_grpc_assertions_ordered_statuses_limits_remaining_and_reset() {
        let Some(server) = TestRedisServer::start() else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{}", server.port)).unwrap();
        let redis = redis::aio::ConnectionManager::new(client).await.unwrap();
        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "users",
                    "value": "alice",
                    "rate_limit": { "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }
                },
                {
                    "key": "orgs",
                    "value": "acme",
                    "rate_limit": { "algorithm": "fixed_window", "unit": "minutes", "requests_per_unit": 100 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        // Request 1: 4 hits on users=alice, 15 hits on orgs=acme
        let req1 = RateLimitRequest {
            domain: "default".to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "users".to_string(),
                        value: "alice".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(4),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "orgs".to_string(),
                        value: "acme".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(15),
                    is_negative_hits: false,
                },
            ],
            hits_addend: 0,
        };

        let resp1 = steward
            .should_rate_limit(tonic::Request::new(req1))
            .await
            .expect("should_rate_limit succeeded")
            .into_inner();

        assert_eq!(resp1.overall_code, Code::Ok as i32);
        assert_eq!(
            resp1.statuses.len(),
            2,
            "must maintain strict 1:1 descriptor ordering"
        );

        // Status 0: users=alice (limit: 10/s, remaining: 6)
        let s0 = &resp1.statuses[0];
        assert_eq!(s0.code, Code::Ok as i32);
        let lim0 = s0.current_limit.as_ref().expect("current_limit present");
        assert_eq!(lim0.requests_per_unit, 10);
        assert_eq!(lim0.unit, 1); // Second
        assert_eq!(s0.limit_remaining, 6);
        let reset0 = s0
            .duration_until_reset
            .as_ref()
            .expect("duration_until_reset present");
        assert!(reset0.seconds > 0 && reset0.seconds <= 1);
        assert_eq!(reset0.nanos, 0);

        // Status 1: orgs=acme (limit: 100/min, remaining: 85)
        let s1 = &resp1.statuses[1];
        assert_eq!(s1.code, Code::Ok as i32);
        let lim1 = s1.current_limit.as_ref().expect("current_limit present");
        assert_eq!(lim1.requests_per_unit, 100);
        assert_eq!(lim1.unit, 2); // Minute
        assert_eq!(s1.limit_remaining, 85);
        let reset1 = s1
            .duration_until_reset
            .as_ref()
            .expect("duration_until_reset present");
        assert!(reset1.seconds > 0 && reset1.seconds <= 60);
        assert_eq!(reset1.nanos, 0);

        // Request 2: additional 7 hits on users=alice (4 + 7 = 11 > 10 => OVER_LIMIT), 5 hits on orgs=acme (15 + 5 = 20 <= 100)
        let req2 = RateLimitRequest {
            domain: "default".to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "users".to_string(),
                        value: "alice".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(7),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "orgs".to_string(),
                        value: "acme".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(5),
                    is_negative_hits: false,
                },
            ],
            hits_addend: 0,
        };

        let resp2 = steward
            .should_rate_limit(tonic::Request::new(req2))
            .await
            .expect("should_rate_limit succeeded")
            .into_inner();

        assert_eq!(resp2.overall_code, Code::OverLimit as i32);
        assert_eq!(resp2.statuses.len(), 2);

        // Status 0: users=alice is now OVER_LIMIT
        let s0_2 = &resp2.statuses[0];
        assert_eq!(s0_2.code, Code::OverLimit as i32);
        assert_eq!(s0_2.limit_remaining, 0);
        assert!(s0_2.duration_until_reset.is_some());

        // Status 1: orgs=acme is still OK, remaining is 80
        let s1_2 = &resp2.statuses[1];
        assert_eq!(s1_2.code, Code::Ok as i32);
        assert_eq!(s1_2.limit_remaining, 80);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn redis_error_returns_unavailable_when_no_rule_over_limit() {
        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "users",
                    "value": "alice",
                    "rate_limit": { "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);

        // Use a client pointing to an unreachable port to guarantee connection error
        let client = redis::Client::open("redis://127.0.0.1:1").unwrap();
        let mut config = redis::aio::ConnectionManagerConfig::new();
        config = config.set_connection_timeout(Some(std::time::Duration::from_millis(50)));
        config = config.set_number_of_retries(1);
        let redis = redis::aio::ConnectionManager::new_lazy_with_config(client, config).unwrap();

        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: std::time::Duration::from_secs(1),
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let req = RateLimitRequest {
            domain: "default".to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "users".to_string(),
                    value: "alice".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        let err = steward
            .should_rate_limit(tonic::Request::new(req))
            .await
            .expect_err("should return gRPC error when backend is unavailable");

        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert_eq!(err.message(), "rate limit storage backend is unavailable");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn definitive_denial_precedence_when_rule1_over_limit_and_rule2_redis_error() {
        let Some(server) = TestRedisServer::start() else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{}", server.port)).unwrap();
        let redis = redis::aio::ConnectionManager::new(client.clone())
            .await
            .unwrap();
        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "tier",
                    "value": "premium",
                    "rate_limit": { "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 5 }
                },
                {
                    "key": "tier",
                    "value": "standard",
                    "rate_limit": { "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 5 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let mut conn = client.get_connection().unwrap();

        // 1. Prime rule 1 (tier=premium) so it exceeds the limit (SET to 5, next hit will be 6 > 5 -> OVER_LIMIT)
        let domain_policy = steward.config_rx.borrow().get("default").unwrap().clone();
        let premium_match = domain_policy.match_entries(&[("tier", "premium")]).unwrap();
        let premium_limit = premium_match.rate_limits[0];
        let premium_path = encode_canonical_path([("tier", "premium")]);
        let premium_key = rate_limit_key(
            "default",
            premium_match.policy_id,
            &premium_path,
            &premium_limit,
        );
        let _: () = redis::cmd("SET")
            .arg(&premium_key)
            .arg(5)
            .query(&mut conn)
            .unwrap();

        // 2. Corrupt rule 2 (tier=standard) key by creating a Hash at this key, so INCRBY in fixed_window.lua throws WRONGTYPE error
        let standard_match = domain_policy
            .match_entries(&[("tier", "standard")])
            .unwrap();
        let standard_limit = standard_match.rate_limits[0];
        let standard_path = encode_canonical_path([("tier", "standard")]);
        let standard_key = rate_limit_key(
            "default",
            standard_match.policy_id,
            &standard_path,
            &standard_limit,
        );
        let _: () = redis::cmd("HSET")
            .arg(&standard_key)
            .arg("corrupt_field")
            .arg("corrupt_val")
            .query(&mut conn)
            .unwrap();

        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        // Request with both descriptors: Rule 1 evaluates to OVER_LIMIT, Rule 2 triggers Redis WRONGTYPE error
        let req = RateLimitRequest {
            domain: "default".to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "tier".to_string(),
                        value: "premium".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "tier".to_string(),
                        value: "standard".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
            ],
            hits_addend: 0,
        };

        let resp = steward
            .should_rate_limit(tonic::Request::new(req))
            .await
            .expect("Rule 1 OVER_LIMIT must take precedence over Rule 2 Redis error and return OK response")
            .into_inner();

        // F06 Rule 1: Definitive denial WINS!
        assert_eq!(resp.overall_code, Code::OverLimit as i32);
        assert_eq!(resp.statuses.len(), 2);
        assert_eq!(resp.statuses[0].code, Code::OverLimit as i32);
        assert_eq!(resp.statuses[0].limit_remaining, 0);

        // Clean up
        let _: () = redis::cmd("DEL")
            .arg(&premium_key)
            .arg(&standard_key)
            .query(&mut conn)
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unmatched_descriptors_populate_unconstrained_status_code_ok_limit_none() {
        let Some(server) = TestRedisServer::start() else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{}", server.port)).unwrap();
        let redis = redis::aio::ConnectionManager::new(client).await.unwrap();
        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "known_key",
                    "value": "known_val",
                    "rate_limit": { "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let req = RateLimitRequest {
            domain: "default".to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "known_key".to_string(),
                        value: "known_val".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "unknown_key".to_string(),
                        value: "unknown_val".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
            ],
            hits_addend: 0,
        };

        let resp = steward
            .should_rate_limit(tonic::Request::new(req))
            .await
            .expect("request should succeed")
            .into_inner();

        assert_eq!(resp.overall_code, Code::Ok as i32);
        assert_eq!(resp.statuses.len(), 2);

        // Matched descriptor
        assert_eq!(resp.statuses[0].code, Code::Ok as i32);
        assert!(resp.statuses[0].current_limit.is_some());

        // Unmatched descriptor: Code::Ok, limit = None, limit_remaining = 0, duration_until_reset = None
        assert_eq!(resp.statuses[1].code, Code::Ok as i32);
        assert_eq!(resp.statuses[1].current_limit, None);
        assert_eq!(resp.statuses[1].limit_remaining, 0);
        assert_eq!(resp.statuses[1].duration_until_reset, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unconfigured_domain_returns_unconstrained_status_for_all_descriptors() {
        let json_str = r#"{
            "domain": "configured_domain",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward::for_test(rx).await;

        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let req = RateLimitRequest {
            domain: "unconfigured_domain".to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "k1".to_string(),
                        value: "v1".to_string(),
                    }],
                    limit: None,
                    hits_addend: None,
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "k2".to_string(),
                        value: "v2".to_string(),
                    }],
                    limit: None,
                    hits_addend: None,
                    is_negative_hits: false,
                },
            ],
            hits_addend: 0,
        };

        let resp = steward
            .should_rate_limit(tonic::Request::new(req))
            .await
            .expect("unconfigured domain allows traffic")
            .into_inner();

        assert_eq!(resp.overall_code, Code::Ok as i32);
        assert_eq!(resp.statuses.len(), 2);
        for status in &resp.statuses {
            assert_eq!(status.code, Code::Ok as i32);
            assert_eq!(status.current_limit, None);
            assert_eq!(status.limit_remaining, 0);
            assert_eq!(status.duration_until_reset, None);
        }
    }

    #[test]
    fn test_is_redis_timeout_classification() {
        use super::is_redis_timeout;

        let timeout_err = redis::RedisError::from((redis::ErrorKind::Io, "operation timed out"));
        assert!(is_redis_timeout(&timeout_err));

        let query_timeout_err =
            redis::RedisError::from((redis::ErrorKind::Io, "command timed out"));
        assert!(is_redis_timeout(&query_timeout_err));

        // Pool connection acquisition failure (e.g. Redis unreachable or stopped) is classified
        // as a backend connection error (Unavailable), not a timeout.
        let pool_conn_err = redis::RedisError::from((
            redis::ErrorKind::Io,
            "failed to acquire Redis connection",
            "timed out waiting to open a connection".to_string(),
        ));
        assert!(!is_redis_timeout(&pool_conn_err));

        let conn_refused = redis::RedisError::from((
            redis::ErrorKind::Io,
            "failed to acquire Redis connection",
            "connection refused".to_string(),
        ));
        assert!(!is_redis_timeout(&conn_refused));

        let client_err = redis::RedisError::from((
            redis::ErrorKind::Client,
            "WRONGTYPE Operation against a key holding the wrong kind of value",
        ));
        assert!(!is_redis_timeout(&client_err));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_window_exact_quota_boundary_assertions() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "domain_exact_boundary";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "tier",
                    "value": "gold",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 5 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let make_request = || RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "tier".to_string(),
                    value: "gold".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // Assert requests 1..=capacity (1..=5) return Code::Ok with strictly decrementing limit_remaining
        for i in 1..=5 {
            let resp = steward
                .should_rate_limit(tonic::Request::new(make_request()))
                .await
                .expect("should_rate_limit succeeded")
                .into_inner();

            assert_eq!(
                resp.overall_code,
                Code::Ok as i32,
                "request {i} should be Ok"
            );
            assert_eq!(resp.statuses.len(), 1);
            let s = &resp.statuses[0];
            assert_eq!(
                s.code,
                Code::Ok as i32,
                "status for request {i} should be Ok"
            );
            assert_eq!(
                s.limit_remaining,
                (5 - i) as u32,
                "remaining quota after request {i} should be {}",
                5 - i
            );
            let lim = s.current_limit.as_ref().expect("current limit present");
            assert_eq!(lim.requests_per_unit, 5);
            assert_eq!(lim.unit, 1); // Second
            assert!(s.duration_until_reset.is_some());
        }

        // Request capacity + 1 (request 6) returns Code::OverLimit with limit_remaining == 0
        let resp6 = steward
            .should_rate_limit(tonic::Request::new(make_request()))
            .await
            .expect("should_rate_limit succeeded")
            .into_inner();

        assert_eq!(
            resp6.overall_code,
            Code::OverLimit as i32,
            "request 6 must be OverLimit"
        );
        assert_eq!(resp6.statuses.len(), 1);
        let s6 = &resp6.statuses[0];
        assert_eq!(s6.code, Code::OverLimit as i32);
        assert_eq!(s6.limit_remaining, 0);
        let lim6 = s6.current_limit.as_ref().expect("current limit present");
        assert_eq!(lim6.requests_per_unit, 5);
        assert!(s6.duration_until_reset.is_some());

        // Subsequent request (request 7) also returns OverLimit
        let resp7 = steward
            .should_rate_limit(tonic::Request::new(make_request()))
            .await
            .expect("should_rate_limit succeeded")
            .into_inner();
        assert_eq!(resp7.overall_code, Code::OverLimit as i32);
        assert_eq!(resp7.statuses[0].code, Code::OverLimit as i32);
        assert_eq!(resp7.statuses[0].limit_remaining, 0);

        // Verify exact counter in Redis
        let mut conn = client.get_connection().unwrap();
        let domain_policy = steward.config_rx.borrow().get(domain).unwrap().clone();
        let matched = domain_policy.match_entries(&[("tier", "gold")]).unwrap();
        let path = encode_canonical_path([("tier", "gold")]);
        let rkey = rate_limit_key(domain, matched.policy_id, &path, &matched.rate_limits[0]);
        let count: i64 = redis::cmd("GET").arg(&rkey).query(&mut conn).unwrap();
        assert_eq!(count, 7, "Redis counter must record exact hit count of 7");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_window_expiry_resets_quota_after_duration() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "domain_window_expiry";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "endpoint",
                    "value": "login",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 2 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, _client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let make_request = || RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "endpoint".to_string(),
                    value: "login".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // Consume quota: 1st hit -> OK, remaining 1
        let resp1 = steward
            .should_rate_limit(tonic::Request::new(make_request()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp1.overall_code, Code::Ok as i32);
        assert_eq!(resp1.statuses[0].limit_remaining, 1);

        // 2nd hit -> OK, remaining 0
        let resp2 = steward
            .should_rate_limit(tonic::Request::new(make_request()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp2.overall_code, Code::Ok as i32);
        assert_eq!(resp2.statuses[0].limit_remaining, 0);

        // 3rd hit -> OVER_LIMIT
        let resp3 = steward
            .should_rate_limit(tonic::Request::new(make_request()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp3.overall_code, Code::OverLimit as i32);
        assert_eq!(resp3.statuses[0].code, Code::OverLimit as i32);

        // Wait for the 1-second fixed window to expire
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        // After window expiry, quota has reset. Request succeeds with fresh budget.
        let resp_reset = steward
            .should_rate_limit(tonic::Request::new(make_request()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            resp_reset.overall_code,
            Code::Ok as i32,
            "quota must reset after window duration expires"
        );
        assert_eq!(resp_reset.statuses[0].code, Code::Ok as i32);
        assert_eq!(
            resp_reset.statuses[0].limit_remaining, 1,
            "remaining must reset to capacity - 1 = 1"
        );

        // Next hit in the new window -> remaining 0
        let resp_reset2 = steward
            .should_rate_limit(tonic::Request::new(make_request()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp_reset2.overall_code, Code::Ok as i32);
        assert_eq!(resp_reset2.statuses[0].limit_remaining, 0);

        // Subsequent hit -> OVER_LIMIT
        let resp_reset3 = steward
            .should_rate_limit(tonic::Request::new(make_request()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp_reset3.overall_code, Code::OverLimit as i32);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_window_hierarchy_ordering_and_tenant_isolation() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "domain_hierarchy_ordering";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "A",
                    "value": "1",
                    "descriptors": [
                        {{
                            "key": "B",
                            "value": "2",
                            "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 2 }}
                        }}
                    ]
                }},
                {{
                    "key": "B",
                    "value": "2",
                    "descriptors": [
                        {{
                            "key": "A",
                            "value": "1",
                            "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 5 }}
                        }}
                    ]
                }},
                {{
                    "key": "tenant",
                    "value": "alpha",
                    "descriptors": [
                        {{
                            "key": "route",
                            "value": "/payments",
                            "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 2 }}
                        }}
                    ]
                }},
                {{
                    "key": "tenant",
                    "value": "beta",
                    "descriptors": [
                        {{
                            "key": "route",
                            "value": "/payments",
                            "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 2 }}
                        }}
                    ]
                }}
            ]
        }}"#
        );

        let Some((_server, steward, _client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        // 1. Hierarchy & Ordering: [(A, 1), (B, 2)] vs [(B, 2), (A, 1)]
        let req_ab = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![
                    Entry {
                        key: "A".to_string(),
                        value: "1".to_string(),
                    },
                    Entry {
                        key: "B".to_string(),
                        value: "2".to_string(),
                    },
                ],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        let req_ba = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![
                    Entry {
                        key: "B".to_string(),
                        value: "2".to_string(),
                    },
                    Entry {
                        key: "A".to_string(),
                        value: "1".to_string(),
                    },
                ],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // Exhaust [(A, 1), (B, 2)] capacity of 2
        let r1 = steward
            .should_rate_limit(tonic::Request::new(req_ab.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r1.overall_code, Code::Ok as i32);
        assert_eq!(r1.statuses[0].limit_remaining, 1);

        let r2 = steward
            .should_rate_limit(tonic::Request::new(req_ab.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r2.overall_code, Code::Ok as i32);
        assert_eq!(r2.statuses[0].limit_remaining, 0);

        let r3 = steward
            .should_rate_limit(tonic::Request::new(req_ab.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r3.overall_code, Code::OverLimit as i32);
        assert_eq!(r3.statuses[0].code, Code::OverLimit as i32);

        // [(B, 2), (A, 1)] has not been touched, remaining capacity is 5 - 1 = 4
        let r_ba = steward
            .should_rate_limit(tonic::Request::new(req_ba.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            r_ba.overall_code,
            Code::Ok as i32,
            "[(B, 2), (A, 1)] must be isolated from [(A, 1), (B, 2)]"
        );
        assert_eq!(r_ba.statuses[0].code, Code::Ok as i32);
        assert_eq!(r_ba.statuses[0].limit_remaining, 4);
        assert_eq!(
            r_ba.statuses[0]
                .current_limit
                .as_ref()
                .unwrap()
                .requests_per_unit,
            5
        );

        // 2. Tenant isolation: tenant alpha vs tenant beta
        let req_alpha = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![
                    Entry {
                        key: "tenant".to_string(),
                        value: "alpha".to_string(),
                    },
                    Entry {
                        key: "route".to_string(),
                        value: "/payments".to_string(),
                    },
                ],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        let req_beta = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![
                    Entry {
                        key: "tenant".to_string(),
                        value: "beta".to_string(),
                    },
                    Entry {
                        key: "route".to_string(),
                        value: "/payments".to_string(),
                    },
                ],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // Exhaust alpha's limit of 2
        let _ = steward
            .should_rate_limit(tonic::Request::new(req_alpha.clone()))
            .await
            .unwrap();
        let _ = steward
            .should_rate_limit(tonic::Request::new(req_alpha.clone()))
            .await
            .unwrap();
        let alpha_denied = steward
            .should_rate_limit(tonic::Request::new(req_alpha.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(alpha_denied.overall_code, Code::OverLimit as i32);

        // Beta still has full budget (capacity 2, first hit gives remaining 1)
        let beta_ok = steward
            .should_rate_limit(tonic::Request::new(req_beta.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            beta_ok.overall_code,
            Code::Ok as i32,
            "tenant beta must be isolated from tenant alpha"
        );
        assert_eq!(beta_ok.statuses[0].limit_remaining, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_window_wildcard_dynamic_counter_creation() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "domain_wildcard_test";
        // Wildcard: value is omitted, matches any dynamic value of client_id
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "client_id",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 2 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let make_client_req = |id: &str| RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "client_id".to_string(),
                    value: id.to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // Client 1 sends 2 requests (capacity 2) -> OK, then 3rd -> OVER_LIMIT
        let r1_1 = steward
            .should_rate_limit(tonic::Request::new(make_client_req("client_1")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r1_1.overall_code, Code::Ok as i32);
        assert_eq!(r1_1.statuses[0].limit_remaining, 1);

        let r1_2 = steward
            .should_rate_limit(tonic::Request::new(make_client_req("client_1")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r1_2.overall_code, Code::Ok as i32);
        assert_eq!(r1_2.statuses[0].limit_remaining, 0);

        let r1_3 = steward
            .should_rate_limit(tonic::Request::new(make_client_req("client_1")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r1_3.overall_code, Code::OverLimit as i32);

        // Client 2 (different wildcard value) sends request -> OK, fresh budget
        let r2_1 = steward
            .should_rate_limit(tonic::Request::new(make_client_req("client_2")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            r2_1.overall_code,
            Code::Ok as i32,
            "dynamic wildcard client_2 must have independent budget"
        );
        assert_eq!(r2_1.statuses[0].limit_remaining, 1);

        // Client 3 sends request -> OK, fresh budget
        let r3_1 = steward
            .should_rate_limit(tonic::Request::new(make_client_req("client_3")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r3_1.overall_code, Code::Ok as i32);
        assert_eq!(r3_1.statuses[0].limit_remaining, 1);

        // Check Redis keys: verify 3 distinct keys exist in Redis with expected counts
        let mut conn = client.get_connection().unwrap();
        let domain_policy = steward.config_rx.borrow().get(domain).unwrap().clone();
        for (client_id, expected_count) in [("client_1", 3), ("client_2", 1), ("client_3", 1)] {
            let matched = domain_policy
                .match_entries(&[("client_id", client_id)])
                .unwrap();
            let path = encode_canonical_path([("client_id", client_id)]);
            let rkey = rate_limit_key(domain, matched.policy_id, &path, &matched.rate_limits[0]);
            let actual: i64 = redis::cmd("GET").arg(&rkey).query(&mut conn).unwrap();
            assert_eq!(
                actual, expected_count,
                "Redis count for {client_id} must be {expected_count}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_window_override_application_and_validation() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor,
            rate_limit_descriptor::{Entry, RateLimitOverride},
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "domain_override_test";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "tier",
                    "value": "silver",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 100 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, _client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        // 1. Valid override: override capacity to 2 (from configured 100)
        let make_override_req = |override_cap: u32, override_unit: i32| RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "tier".to_string(),
                    value: "silver".to_string(),
                }],
                limit: Some(RateLimitOverride {
                    requests_per_unit: override_cap,
                    unit: override_unit,
                }),
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        let r1 = steward
            .should_rate_limit(tonic::Request::new(make_override_req(2, 1)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r1.overall_code, Code::Ok as i32);
        assert_eq!(r1.statuses[0].code, Code::Ok as i32);
        assert_eq!(r1.statuses[0].limit_remaining, 1);
        assert_eq!(
            r1.statuses[0]
                .current_limit
                .as_ref()
                .unwrap()
                .requests_per_unit,
            2
        );

        let r2 = steward
            .should_rate_limit(tonic::Request::new(make_override_req(2, 1)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r2.overall_code, Code::Ok as i32);
        assert_eq!(r2.statuses[0].limit_remaining, 0);

        let r3 = steward
            .should_rate_limit(tonic::Request::new(make_override_req(2, 1)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            r3.overall_code,
            Code::OverLimit as i32,
            "override capacity of 2 should be enforced"
        );
        assert_eq!(r3.statuses[0].code, Code::OverLimit as i32);
        assert_eq!(r3.statuses[0].limit_remaining, 0);

        // 2. Invalid override: requests_per_unit = 0 -> InvalidArgument error
        let err_zero = steward
            .should_rate_limit(tonic::Request::new(make_override_req(0, 1)))
            .await
            .unwrap_err();
        assert_eq!(err_zero.code(), tonic::Code::InvalidArgument);
        assert!(err_zero.message().contains("greater than 0"));

        // 3. Invalid override: unit = Unknown (0) -> InvalidArgument error
        let err_unit = steward
            .should_rate_limit(tonic::Request::new(make_override_req(10, 0)))
            .await
            .unwrap_err();
        assert_eq!(err_unit.code(), tonic::Code::InvalidArgument);
        assert!(err_unit.message().contains("invalid or unknown"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_window_weighted_hits_increment_counter_by_exact_cost() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "domain_weighted_hits";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "api",
                    "value": "compute",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let make_weighted_req = |cost: u64| RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "api".to_string(),
                    value: "compute".to_string(),
                }],
                limit: None,
                hits_addend: Some(cost),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // Hit 1: cost = 4 -> allowed, remaining 10 - 4 = 6
        let r1 = steward
            .should_rate_limit(tonic::Request::new(make_weighted_req(4)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r1.overall_code, Code::Ok as i32);
        assert_eq!(r1.statuses[0].limit_remaining, 6);

        // Hit 2: cost = 5 -> allowed, remaining 6 - 5 = 1
        let r2 = steward
            .should_rate_limit(tonic::Request::new(make_weighted_req(5)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r2.overall_code, Code::Ok as i32);
        assert_eq!(r2.statuses[0].limit_remaining, 1);

        // Hit 3: cost = 3 -> 9 + 3 = 12 > 10 -> OVER_LIMIT, remaining 0
        let r3 = steward
            .should_rate_limit(tonic::Request::new(make_weighted_req(3)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r3.overall_code, Code::OverLimit as i32);
        assert_eq!(r3.statuses[0].code, Code::OverLimit as i32);
        assert_eq!(r3.statuses[0].limit_remaining, 0);

        // Verify Redis counter holds exact cost 4 + 5 + 3 = 12
        let mut conn = client.get_connection().unwrap();
        let domain_policy = steward.config_rx.borrow().get(domain).unwrap().clone();
        let matched = domain_policy.match_entries(&[("api", "compute")]).unwrap();
        let path = encode_canonical_path([("api", "compute")]);
        let rkey = rate_limit_key(domain, matched.policy_id, &path, &matched.rate_limits[0]);
        let count: i64 = redis::cmd("GET").arg(&rkey).query(&mut conn).unwrap();
        assert_eq!(
            count, 12,
            "Redis counter must increment by exact cost sum of 12"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_window_duplicate_descriptors_charged_additively_and_preserve_order() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "domain_duplicate_desc";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "service",
                    "value": "auth",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        // Single request with two identical descriptors:
        // desc 0: cost 3
        // desc 1: cost 4
        let req1 = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "service".to_string(),
                        value: "auth".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(3),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "service".to_string(),
                        value: "auth".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(4),
                    is_negative_hits: false,
                },
            ],
            hits_addend: 0,
        };

        let resp1 = steward
            .should_rate_limit(tonic::Request::new(req1))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp1.overall_code, Code::Ok as i32);
        assert_eq!(
            resp1.statuses.len(),
            2,
            "must maintain 1:1 descriptor status correspondence"
        );

        // Status 0 corresponds to descriptor 0 (charged 3 hits -> remaining 7)
        assert_eq!(resp1.statuses[0].code, Code::Ok as i32);
        assert_eq!(resp1.statuses[0].limit_remaining, 7);

        // Status 1 corresponds to descriptor 1 (charged 4 hits additively -> remaining 3)
        assert_eq!(resp1.statuses[1].code, Code::Ok as i32);
        assert_eq!(resp1.statuses[1].limit_remaining, 3);

        // Follow-up request with duplicate descriptors that exceeds the limit:
        // desc 0: cost 2 (7 + 2 = 9 <= 10 -> OK)
        // desc 1: cost 3 (9 + 3 = 12 > 10 -> OVER_LIMIT)
        let req2 = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "service".to_string(),
                        value: "auth".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(2),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "service".to_string(),
                        value: "auth".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(3),
                    is_negative_hits: false,
                },
            ],
            hits_addend: 0,
        };

        let resp2 = steward
            .should_rate_limit(tonic::Request::new(req2))
            .await
            .unwrap()
            .into_inner();
        // Definitive denial wins overall response
        assert_eq!(resp2.overall_code, Code::OverLimit as i32);
        assert_eq!(resp2.statuses.len(), 2);

        // Status 0: OK, remaining 1 (10 - 9 = 1)
        assert_eq!(resp2.statuses[0].code, Code::Ok as i32);
        assert_eq!(resp2.statuses[0].limit_remaining, 1);

        // Status 1: OverLimit, remaining 0
        assert_eq!(resp2.statuses[1].code, Code::OverLimit as i32);
        assert_eq!(resp2.statuses[1].limit_remaining, 0);

        // Total hits in Redis: 3 + 4 + 2 + 3 = 12
        let mut conn = client.get_connection().unwrap();
        let domain_policy = steward.config_rx.borrow().get(domain).unwrap().clone();
        let matched = domain_policy.match_entries(&[("service", "auth")]).unwrap();
        let path = encode_canonical_path([("service", "auth")]);
        let rkey = rate_limit_key(domain, matched.policy_id, &path, &matched.rate_limits[0]);
        let count: i64 = redis::cmd("GET").arg(&rkey).query(&mut conn).unwrap();
        assert_eq!(count, 12);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_redis_killed_mid_flight_returns_unavailable() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        // Spawn a dedicated ephemeral Redis server instance
        let Some((port, mut child)) = spawn_dedicated_ephemeral_redis() else {
            eprintln!("Skipping test: redis-server command not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{port}")).unwrap();
        let mut config = redis::aio::ConnectionManagerConfig::new();
        config = config.set_connection_timeout(Some(std::time::Duration::from_millis(150)));
        config = config.set_response_timeout(Some(std::time::Duration::from_millis(150)));
        config = config.set_number_of_retries(1);
        let redis = redis::aio::ConnectionManager::new_with_config(client, config)
            .await
            .unwrap();

        let domain = "domain_kill_test";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "search",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(&config_json).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let make_req = || RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "action".to_string(),
                    value: "search".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // First request succeeds while Redis is healthy
        let r1 = steward
            .should_rate_limit(tonic::Request::new(make_req()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r1.overall_code, Code::Ok as i32);

        // Kill the dedicated ephemeral Redis process
        let _ = child.kill();
        let _ = child.wait();

        // Give connection a moment to close
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Subsequent request fails with tonic::Code::Unavailable
        let err = steward
            .should_rate_limit(tonic::Request::new(make_req()))
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::Unavailable,
            "backend failure when Redis is killed must return Unavailable for Envoy failure_mode_deny handling"
        );
        assert_eq!(err.message(), "rate limit storage backend is unavailable");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_redis_failover_and_reconnection_resumes_cleanly() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let Some((port, mut child)) = spawn_dedicated_ephemeral_redis() else {
            eprintln!("Skipping test: redis-server command not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{port}")).unwrap();
        let mut config = redis::aio::ConnectionManagerConfig::new();
        config = config.set_connection_timeout(Some(std::time::Duration::from_millis(200)));
        config = config.set_response_timeout(Some(std::time::Duration::from_millis(200)));
        config = config.set_number_of_retries(2);
        let redis = redis::aio::ConnectionManager::new_with_config(client, config)
            .await
            .unwrap();

        let domain = "domain_reconnect_test";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "checkout",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 100 }}
                }}
            ]
        }}"#
        );

        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(&config_json).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let make_req = || RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "action".to_string(),
                    value: "checkout".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // Phase 1: Request succeeds on initial healthy primary
        let r1 = steward
            .should_rate_limit(tonic::Request::new(make_req()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r1.overall_code, Code::Ok as i32);

        // Phase 2: Kill primary Redis process (simulating outage or crash)
        let _ = child.kill();
        let _ = child.wait();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let err = steward
            .should_rate_limit(tonic::Request::new(make_req()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);

        // Phase 3: Simulate failover by starting a new Redis instance on the same target endpoint/port
        let mut new_child = spawn_dedicated_ephemeral_redis_on_port(port).unwrap();

        // Allow ConnectionManager to re-establish connection
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Phase 4: Request immediately resumes succeeding with zero restart needed
        let r2 = steward
            .should_rate_limit(tonic::Request::new(make_req()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r2.overall_code, Code::Ok as i32);

        let _ = new_child.kill();
        let _ = new_child.wait();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_replica_http2_load_balancing_and_timeout() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::{
            RateLimitRequest, RateLimitResponse,
            rate_limit_service_client::RateLimitServiceClient,
            rate_limit_service_server::{RateLimitService, RateLimitServiceServer},
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tonic::transport::{Channel, Endpoint, Server};

        #[derive(Clone)]
        struct CountingService {
            steward: super::Steward,
            counter: std::sync::Arc<AtomicUsize>,
            delay: Option<std::time::Duration>,
        }

        #[tonic::async_trait]
        impl RateLimitService for CountingService {
            async fn should_rate_limit(
                &self,
                request: tonic::Request<RateLimitRequest>,
            ) -> Result<tonic::Response<RateLimitResponse>, tonic::Status> {
                self.counter.fetch_add(1, Ordering::SeqCst);
                if let Some(delay) = self.delay {
                    tokio::time::sleep(delay).await;
                }
                self.steward.should_rate_limit(request).await
            }
        }

        let Some((redis_port, mut redis_child)) = spawn_dedicated_ephemeral_redis() else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let domain = "domain_lb_test";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "endpoint",
                    "value": "api",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 1000 }}
                }}
            ]
        }}"#
        );
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(&config_json).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);

        let make_steward = || async {
            let client = redis::Client::open(format!("redis://127.0.0.1:{redis_port}")).unwrap();
            let redis = redis::aio::ConnectionManager::new(client).await.unwrap();
            super::Steward {
                config_rx: rx.clone(),
                redis,
                metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                    "",
                    cadence::NopMetricSink,
                )),
                scripts: super::StewardScripts::default(),
                execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
                admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1024)),
                in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            }
        };

        let s1 = make_steward().await;
        let s2 = make_steward().await;

        let counter_1 = std::sync::Arc::new(AtomicUsize::new(0));
        let counter_2 = std::sync::Arc::new(AtomicUsize::new(0));

        let listener_1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_1 = listener_1.local_addr().unwrap().port();
        let incoming_1 = tokio_stream::wrappers::TcpListenerStream::new(listener_1);

        let listener_2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_2 = listener_2.local_addr().unwrap().port();
        let incoming_2 = tokio_stream::wrappers::TcpListenerStream::new(listener_2);

        let svc_1 = CountingService {
            steward: s1,
            counter: counter_1.clone(),
            delay: None,
        };
        let svc_2 = CountingService {
            steward: s2,
            counter: counter_2.clone(),
            delay: None,
        };

        let srv_handle_1 = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(RateLimitServiceServer::new(svc_1))
                .serve_with_incoming(incoming_1)
                .await;
        });

        let srv_handle_2 = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(RateLimitServiceServer::new(svc_2))
                .serve_with_incoming(incoming_2)
                .await;
        });

        // Give servers a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 1. Multi-Replica HTTP/2 Load Balancing Verification
        let ep1 = Endpoint::from_shared(format!("http://127.0.0.1:{port_1}")).unwrap();
        let ep2 = Endpoint::from_shared(format!("http://127.0.0.1:{port_2}")).unwrap();
        let channel = Channel::balance_list(vec![ep1, ep2].into_iter());
        let mut client = RateLimitServiceClient::new(channel);

        let make_req = || RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "endpoint".to_string(),
                    value: "api".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        for _ in 0..60 {
            let resp = client
                .should_rate_limit(tonic::Request::new(make_req()))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(resp.overall_code, Code::Ok as i32);
        }

        let hits_1 = counter_1.load(Ordering::SeqCst);
        let hits_2 = counter_2.load(Ordering::SeqCst);
        assert_eq!(hits_1 + hits_2, 60, "total requests must equal 60");
        assert!(
            hits_1 >= 20 && hits_2 >= 20,
            "requests must distribute evenly across replicas under HTTP/2: hits_1={hits_1}, hits_2={hits_2}"
        );

        // 2. Client RPC Timeout Enforcement (matching Envoy 20ms timeout)
        let listener_slow = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_slow = listener_slow.local_addr().unwrap().port();
        let incoming_slow = tokio_stream::wrappers::TcpListenerStream::new(listener_slow);
        let s_slow = make_steward().await;
        let counter_slow = std::sync::Arc::new(AtomicUsize::new(0));
        let svc_slow = CountingService {
            steward: s_slow,
            counter: counter_slow,
            delay: Some(std::time::Duration::from_millis(150)), // Stalled check > 20ms
        };
        let srv_handle_slow = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(RateLimitServiceServer::new(svc_slow))
                .serve_with_incoming(incoming_slow)
                .await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let ep_slow = Endpoint::from_shared(format!("http://127.0.0.1:{port_slow}"))
            .unwrap()
            .timeout(std::time::Duration::from_millis(20)); // Envoy 20ms timeout
        let channel_slow = ep_slow.connect().await.unwrap();
        let mut timed_client = RateLimitServiceClient::new(channel_slow);

        let start = std::time::Instant::now();
        let timeout_res = timed_client
            .should_rate_limit(tonic::Request::new(make_req()))
            .await;
        let elapsed = start.elapsed();

        assert!(
            timeout_res.is_err(),
            "stalled check must be aborted by client timeout"
        );
        let err = timeout_res.unwrap_err();
        assert!(
            err.code() == tonic::Code::Cancelled || err.code() == tonic::Code::DeadlineExceeded,
            "timed out request must return Cancelled or DeadlineExceeded, got: {:?}",
            err.code()
        );
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "timeout must trigger around 20ms, elapsed was: {elapsed:?}"
        );

        srv_handle_1.abort();
        srv_handle_2.abort();
        srv_handle_slow.abort();
        let _ = redis_child.kill();
        let _ = redis_child.wait();
    }

    #[test]
    fn test_parse_grpc_timeout() {
        assert_eq!(
            super::parse_grpc_timeout("100m"),
            Some(std::time::Duration::from_millis(100))
        );
        assert_eq!(
            super::parse_grpc_timeout("1S"),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            super::parse_grpc_timeout("500000u"),
            Some(std::time::Duration::from_micros(500_000))
        );
        assert_eq!(
            super::parse_grpc_timeout("2H"),
            Some(std::time::Duration::from_secs(7200))
        );
        assert_eq!(
            super::parse_grpc_timeout("10M"),
            Some(std::time::Duration::from_secs(600))
        );
        assert_eq!(
            super::parse_grpc_timeout("1000n"),
            Some(std::time::Duration::from_nanos(1000))
        );

        // Malformed headers
        assert_eq!(super::parse_grpc_timeout(""), None);
        assert_eq!(super::parse_grpc_timeout("abc"), None);
        assert_eq!(super::parse_grpc_timeout("100"), None);
        assert_eq!(super::parse_grpc_timeout("100x"), None);
        assert_eq!(super::parse_grpc_timeout("-5m"), None);
        assert_eq!(super::parse_grpc_timeout("m"), None);
        assert_eq!(super::parse_grpc_timeout("100ms"), None);
        assert_eq!(super::parse_grpc_timeout("invalid"), None);
        assert_eq!(super::parse_grpc_timeout("100 m"), None);
    }

    #[tokio::test]
    async fn test_effective_timeout_calculation() {
        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "action",
                    "value": "search",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let default_config = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(default_config);
        let steward = super::Steward {
            config_rx: rx,
            redis: redis::aio::ConnectionManager::new_lazy_with_config(
                redis::Client::open("redis://127.0.0.1:1").unwrap(),
                redis::aio::ConnectionManagerConfig::default(),
            )
            .unwrap(),
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: std::time::Duration::from_millis(10),
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1024)),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        // No client header -> defaults to execution_timeout (10ms)
        assert_eq!(
            steward.effective_timeout(None),
            std::time::Duration::from_millis(10)
        );

        // Client deadline 100ms -> min(100ms - 2ms, 10ms) = 10ms
        assert_eq!(
            steward.effective_timeout(Some(std::time::Duration::from_millis(100))),
            std::time::Duration::from_millis(10)
        );

        // Client deadline 5ms -> min(5ms - 2ms, 10ms) = 3ms
        assert_eq!(
            steward.effective_timeout(Some(std::time::Duration::from_millis(5))),
            std::time::Duration::from_millis(3)
        );

        // Client deadline 2ms -> min(max(2ms - 2ms, 1ms), 10ms) = 1ms
        assert_eq!(
            steward.effective_timeout(Some(std::time::Duration::from_millis(2))),
            std::time::Duration::from_millis(1)
        );

        // Client deadline 1ms -> min(max(1ms - 2ms, 1ms), 10ms) = 1ms
        assert_eq!(
            steward.effective_timeout(Some(std::time::Duration::from_millis(1))),
            std::time::Duration::from_millis(1)
        );
    }

    #[tokio::test]
    async fn test_global_admission_limit_sheds_excess_load() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "action",
                    "value": "search",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);

        // Configure Steward with 0 available permits in admission semaphore
        let steward = super::Steward::for_test(rx)
            .await
            .with_max_concurrent_requests(0);

        let request = RateLimitRequest {
            domain: "default".to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "action".to_string(),
                    value: "search".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // When semaphore has 0 available permits, request is rejected immediately with ResourceExhausted
        let err = steward
            .should_rate_limit(tonic::Request::new(request))
            .await
            .unwrap_err();

        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        assert_eq!(
            err.message(),
            "admission limit reached; request rejected due to load shedding"
        );
        assert_eq!(steward.admission_semaphore.available_permits(), 0);
    }

    #[tokio::test]
    async fn test_global_admission_limit_permits_retained_during_execution_and_released() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let json_str = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "action",
                    "value": "search",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);

        let steward = super::Steward::for_test(rx)
            .await
            .with_max_concurrent_requests(1);

        assert_eq!(steward.admission_semaphore.available_permits(), 1);

        let request = RateLimitRequest {
            domain: "unconfigured".to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "action".to_string(),
                    value: "search".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        let res = steward
            .should_rate_limit(tonic::Request::new(request))
            .await;
        assert!(res.is_ok());

        // Permit was released upon request completion
        assert_eq!(steward.admission_semaphore.available_permits(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_request_deadline_expiration_returns_deadline_exceeded() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let Some((port, mut child)) = spawn_dedicated_ephemeral_redis() else {
            eprintln!("Skipping test: redis-server command not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{port}")).unwrap();
        let redis = redis::aio::ConnectionManager::new(client.clone())
            .await
            .unwrap();

        let domain = "deadline_test";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "search",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(&config_json).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward {
            config_rx: rx,
            redis,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: std::time::Duration::from_millis(10),
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1024)),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let make_req = || RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "action".to_string(),
                    value: "search".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // First verify normal request succeeds
        let initial_res = steward
            .should_rate_limit(tonic::Request::new(make_req()))
            .await
            .unwrap();
        assert_eq!(initial_res.into_inner().overall_code, Code::Ok as i32);

        // Pause Redis for 2000 ms using a synchronous client connection
        let mut sync_conn = client.get_connection().unwrap();
        let _: () = redis::cmd("CLIENT")
            .arg("PAUSE")
            .arg(2000)
            .query(&mut sync_conn)
            .unwrap();

        // 1. Test internal execution_timeout expiration (10ms expires well before Redis unpauses in 2000ms)
        let req = tonic::Request::new(make_req());
        let start = std::time::Instant::now();
        let err = steward.should_rate_limit(req).await.unwrap_err();
        let elapsed = start.elapsed();

        assert_eq!(err.code(), tonic::Code::DeadlineExceeded);
        assert_eq!(err.message(), "request execution deadline exceeded");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "elapsed was {elapsed:?}"
        );

        // 2. Test grpc-timeout header parsing and enforcement (e.g. 5m -> effective 3ms)
        let mut req_with_timeout = tonic::Request::new(make_req());
        req_with_timeout
            .metadata_mut()
            .insert("grpc-timeout", "5m".parse().unwrap());
        let start = std::time::Instant::now();
        let err = steward
            .should_rate_limit(req_with_timeout)
            .await
            .unwrap_err();
        let elapsed = start.elapsed();

        assert_eq!(err.code(), tonic::Code::DeadlineExceeded);
        assert_eq!(err.message(), "request execution deadline exceeded");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "elapsed was {elapsed:?}"
        );

        // Unpause Redis and kill server
        let _: () = redis::cmd("CLIENT")
            .arg("UNPAUSE")
            .query(&mut sync_conn)
            .unwrap_or(());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sliding_window_concurrent_replicas_same_millisecond_do_not_overwrite_events() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "sliding_window_concurrency";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "endpoint",
                    "value": "checkout",
                    "rate_limit": {{ "algorithm": "sliding_window", "unit": "seconds", "requests_per_unit": 50 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("endpoint", "checkout")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("endpoint", "checkout")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        // Simulate 10 concurrent requests from distinct replicas with hits_addend = 2 (total 20 hits)
        let num_requests = 10;
        let hits_per_req = 2;
        let mut handles = Vec::new();

        for _ in 0..num_requests {
            let steward_clone = steward.clone();
            let req = RateLimitRequest {
                domain: domain.to_string(),
                descriptors: vec![RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "endpoint".to_string(),
                        value: "checkout".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(hits_per_req),
                    is_negative_hits: false,
                }],
                hits_addend: 0,
            };
            handles.push(tokio::spawn(async move {
                steward_clone
                    .should_rate_limit(tonic::Request::new(req))
                    .await
            }));
        }

        for handle in handles {
            let res = handle.await.unwrap().unwrap().into_inner();
            assert_eq!(res.overall_code, Code::Ok as i32);
        }

        // Verify Redis sorted set cardinality is exactly num_requests * hits_per_req = 20
        let card: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(card, (num_requests * hits_per_req) as i64);

        // Verify member format: now_usec:nonce:i where nonce is 32 hex chars
        let members: Vec<String> = redis::cmd("ZRANGE")
            .arg(&key)
            .arg(0)
            .arg(-1)
            .query(&mut conn)
            .unwrap();
        assert_eq!(members.len(), 20);

        let mut seen = std::collections::HashSet::new();
        for m in &members {
            assert!(seen.insert(m.clone()), "member {m} must be unique");
            let parts: Vec<&str> = m.split(':').collect();
            assert_eq!(
                parts.len(),
                3,
                "member {m} must have 3 colon-separated parts"
            );
            assert!(
                parts[0].parse::<u64>().is_ok(),
                "part 0 must be usec timestamp"
            );
            assert_eq!(parts[1].len(), 32, "part 1 must be 32-hex-char nonce");
            let idx = parts[2].parse::<u32>().expect("part 2 must be hit index");
            assert!(idx >= 1 && idx <= hits_per_req as u32);
        }

        // Clean up
        let _: () = redis::cmd("DEL").arg(&key).query(&mut conn).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sliding_window_prunes_expired_entries_and_respects_10000_event_cap() {
        use super::HitOperation;

        let domain = "sliding_window_bounds";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "api",
                    "value": "search",
                    "rate_limit": {{ "algorithm": "sliding_window", "unit": "minutes", "requests_per_unit": 20000 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy.match_entries(&[("api", "search")]).unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("api", "search")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        // Fetch current Redis time
        let redis_time: (i64, i64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now_ms = (redis_time.0 * 1000) + (redis_time.1 / 1000);

        // --- Part A: Prune expired entries ---
        // Insert 5 expired entries with score 120 seconds in the past (> 60s window)
        let expired_score = now_ms - 120_000;
        for i in 0..5 {
            let _: () = redis::cmd("ZADD")
                .arg(&key)
                .arg(expired_score)
                .arg(format!("expired:{i}"))
                .query(&mut conn)
                .unwrap();
        }
        // Insert 3 fresh entries with score now_ms
        for i in 0..3 {
            let _: () = redis::cmd("ZADD")
                .arg(&key)
                .arg(now_ms)
                .arg(format!("fresh:{i}"))
                .query(&mut conn)
                .unwrap();
        }
        assert_eq!(
            redis::cmd("ZCARD")
                .arg(&key)
                .query::<i64>(&mut conn)
                .unwrap(),
            8
        );

        // Execute sliding window check with 2 hits (window = 60s -> expired entries must be pruned)
        let decision = steward
            .check_limit(&key, &limit, HitOperation::Consume(2))
            .await
            .unwrap();
        assert!(decision.allowed);
        // Observed should be 3 fresh + 2 new = 5 (the 5 expired were pruned)
        assert_eq!(decision.observed, 5);
        let card: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(card, 5);

        // --- Part B: Enforce max retention bound of 10,000 events ---
        // Clear key first
        let _: () = redis::cmd("DEL").arg(&key).query(&mut conn).unwrap();

        // Re-fetch authoritative Redis time
        let redis_time: (i64, i64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now_ms = (redis_time.0 * 1000) + (redis_time.1 / 1000);
        let seed_score = now_ms - 5_000; // 5s ago (within 60s window, but older than new hits)

        // Seed sorted set with exactly 10,000 items
        let mut cmd = redis::cmd("ZADD");
        cmd.arg(&key);
        for i in 0..10_000 {
            cmd.arg(seed_score).arg(format!("seed:{i:05}"));
        }
        let _: () = cmd.query(&mut conn).unwrap();
        assert_eq!(
            redis::cmd("ZCARD")
                .arg(&key)
                .query::<i64>(&mut conn)
                .unwrap(),
            10_000
        );

        // Add 10 hits via sliding window script (limit is 20,000, so allowed)
        let decision = steward
            .check_limit(&key, &limit, HitOperation::Consume(10))
            .await
            .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.observed, 10_000);

        // Redis cardinality must be trimmed to exactly 10,000
        let card: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(card, 10_000);

        // Oldest entries (seed:00000 to seed:00009) must have been trimmed by ZREMRANGEBYRANK
        for i in 0..10 {
            let score: Option<f64> = redis::cmd("ZSCORE")
                .arg(&key)
                .arg(format!("seed:{i:05}"))
                .query(&mut conn)
                .unwrap();
            assert!(score.is_none(), "seed:{i:05} must have been trimmed");
        }
        // Entry seed:00010 must still exist
        let score_10: Option<f64> = redis::cmd("ZSCORE")
            .arg(&key)
            .arg("seed:00010")
            .query(&mut conn)
            .unwrap();
        assert!(score_10.is_some(), "seed:00010 must be retained");

        // Clean up
        let _: () = redis::cmd("DEL").arg(&key).query(&mut conn).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn token_bucket_withstands_simulated_clock_regressions_without_extra_refills() {
        use super::HitOperation;

        let domain = "token_bucket_clock_regression";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "transfer",
                    "rate_limit": {{ "algorithm": "token_bucket", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("action", "transfer")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("action", "transfer")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        // 1. Initial consumption: consume 8 tokens out of 10 capacity -> 2 tokens remaining
        let d1 = steward
            .check_limit(&key, &limit, HitOperation::Consume(8))
            .await
            .unwrap();
        assert!(d1.allowed);
        assert_eq!(d1.observed, 2);

        // Read current stored timestamp from Redis
        let state: (Option<f64>, Option<i64>) = redis::cmd("HMGET")
            .arg(&key)
            .arg("tokens")
            .arg("timestamp_ms")
            .query(&mut conn)
            .unwrap();
        let initial_ts = state.1.unwrap();

        // 2. Simulate clock regression: write a future timestamp (initial_ts + 60,000ms) with 2 tokens
        let future_ts = initial_ts + 60_000;
        let _: () = redis::cmd("HSET")
            .arg(&key)
            .arg("tokens")
            .arg(2.0)
            .arg("timestamp_ms")
            .arg(future_ts)
            .query(&mut conn)
            .unwrap();

        // 3. Attempt to consume 3 tokens when Redis TIME is still around initial_ts (< future_ts)
        // With clock drift protection, now_ms is clamped to last (future_ts), elapsed is 0, no refill happens!
        // 2 tokens < 3 cost -> must be rejected
        let d2 = steward
            .check_limit(&key, &limit, HitOperation::Consume(3))
            .await
            .unwrap();
        assert!(
            !d2.allowed,
            "request for 3 tokens should be denied when only 2 tokens exist"
        );
        assert_eq!(d2.observed, 2);

        // Stored timestamp must NOT regress backwards to now_ms
        let state2: (Option<f64>, Option<i64>) = redis::cmd("HMGET")
            .arg(&key)
            .arg("tokens")
            .arg("timestamp_ms")
            .query(&mut conn)
            .unwrap();
        assert_eq!(
            state2.1.unwrap(),
            future_ts,
            "timestamp must not move backwards"
        );

        // 4. Consume 1 token -> should succeed and leave 1 token
        let d3 = steward
            .check_limit(&key, &limit, HitOperation::Consume(1))
            .await
            .unwrap();
        assert!(d3.allowed);
        assert_eq!(d3.observed, 1);

        // Verify stored timestamp is still >= future_ts
        let state3: (Option<f64>, Option<i64>) = redis::cmd("HMGET")
            .arg(&key)
            .arg("tokens")
            .arg("timestamp_ms")
            .query(&mut conn)
            .unwrap();
        assert!(state3.1.unwrap() >= future_ts);

        // Clean up
        let _: () = redis::cmd("DEL").arg(&key).query(&mut conn).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_exceeding_max_sliding_hit_cost_are_rejected() {
        use super::HitOperation;
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "sliding_max_cost";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "endpoint",
                    "value": "batch",
                    "rate_limit": {{ "algorithm": "sliding_window", "unit": "seconds", "requests_per_unit": 500 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("endpoint", "batch")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("endpoint", "batch")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        // 1. gRPC request with hits_addend = 101 must be rejected before Redis execution
        let req1 = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "endpoint".to_string(),
                    value: "batch".to_string(),
                }],
                limit: None,
                hits_addend: None,
                is_negative_hits: false,
            }],
            hits_addend: 101,
        };
        let err1 = steward
            .should_rate_limit(tonic::Request::new(req1))
            .await
            .unwrap_err();
        assert_eq!(err1.code(), tonic::Code::InvalidArgument);
        assert!(err1.message().contains("exceeds maximum of 100"));

        // 2. gRPC request with descriptor hits_addend = 101 must be rejected before Redis execution
        let req2 = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "endpoint".to_string(),
                    value: "batch".to_string(),
                }],
                limit: None,
                hits_addend: Some(101),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };
        let err2 = steward
            .should_rate_limit(tonic::Request::new(req2))
            .await
            .unwrap_err();
        assert_eq!(err2.code(), tonic::Code::InvalidArgument);
        assert!(err2.message().contains("exceeds maximum of 100"));

        // 3. check_limit called directly with hits = 101 on sliding window must be rejected
        let err3 = steward
            .check_limit(&key, &limit, HitOperation::Consume(101))
            .await
            .unwrap_err();
        assert!(err3.to_string().contains("exceeds maximum of 100"));

        // Verify no elements were added to Redis
        let card: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(card, 0);

        // 4. Lua script directly invoked with hits = 101 returns { 0, 0 }
        let mut conn_async = client.get_connection_manager().await.unwrap();
        let outcome: super::ScriptOutcome = steward
            .scripts
            .sliding_window
            .key(&key)
            .arg(10_000)
            .arg(500)
            .arg(101)
            .arg("test-nonce")
            .invoke_async(&mut conn_async)
            .await
            .unwrap();
        assert!(!outcome.allowed);
        assert_eq!(outcome.observed, 0);
    }

    #[test]
    fn script_outcome_strong_typing_and_deserialization_validation() {
        use super::ScriptOutcome;
        use redis::FromRedisValue;

        // 1. Valid allowed outcome: [1, 42]
        let val_allowed = redis::Value::Array(vec![redis::Value::Int(1), redis::Value::Int(42)]);
        let outcome = ScriptOutcome::from_redis_value(val_allowed).expect("must parse [1, 42]");
        assert!(outcome.allowed);
        assert_eq!(outcome.observed, 42);

        // 2. Valid denied outcome: [0, 10]
        let val_denied = redis::Value::Array(vec![redis::Value::Int(0), redis::Value::Int(10)]);
        let outcome = ScriptOutcome::from_redis_value(val_denied).expect("must parse [0, 10]");
        assert!(!outcome.allowed);
        assert_eq!(outcome.observed, 10);

        // 3. Incomplete array with only 1 element -> ParsingError
        let val_short = redis::Value::Array(vec![redis::Value::Int(1)]);
        assert!(ScriptOutcome::from_redis_value(val_short).is_err());

        // 4. Empty array -> ParsingError
        let val_empty = redis::Value::Array(vec![]);
        assert!(ScriptOutcome::from_redis_value(val_empty).is_err());

        // 5. Nil value -> ParsingError
        let val_nil = redis::Value::Nil;
        assert!(ScriptOutcome::from_redis_value(val_nil).is_err());

        // 6. Conversion to Decision
        let decision: super::Decision = outcome.into();
        assert_eq!(decision.allowed, outcome.allowed);
        assert_eq!(decision.observed, outcome.observed);
    }

    #[test]
    fn test_reference_models_pure_logic() {
        use super::{SlidingWindowReference, TokenBucketReference};

        // 1. TokenBucketReference logic
        let mut tb = TokenBucketReference::new(10.0, 10_000);
        assert_eq!(tb.capacity, 10.0);
        assert_eq!(tb.refill_per_ms, 0.001);

        // Initial hit of 4 tokens at t = 1000ms
        let (allowed, observed) = tb.consume(1000, 4);
        assert!(allowed);
        assert_eq!(observed, 6);

        // Simulated backward clock shift to t = 500ms
        // Clock clamping prevents backward movement and elapsed becomes 0
        let (allowed, observed) = tb.consume(500, 2);
        assert!(allowed);
        assert_eq!(observed, 4);
        assert_eq!(tb.last_timestamp_ms, Some(1000));

        // Consume remaining 4 tokens at t = 1000ms
        let (allowed, observed) = tb.consume(1000, 4);
        assert!(allowed);
        assert_eq!(observed, 0);

        // Capacity exhausted, request for 1 token denied
        let (allowed, observed) = tb.consume(1000, 1);
        assert!(!allowed);
        assert_eq!(observed, 0);

        // Continuous fractional refill: advance by 3000ms (3 tokens refilled)
        let (allowed, observed) = tb.consume(4000, 2);
        assert!(allowed);
        assert_eq!(observed, 1);

        // Refund 5 tokens (capped at capacity 10.0)
        let (refunded, observed) = tb.refund(4000, 5);
        assert!(refunded);
        assert_eq!(observed, 6);

        // Dynamic capacity update
        tb.update_capacity(20.0);
        assert_eq!(tb.capacity, 20.0);
        assert_eq!(tb.refill_per_ms, 0.002);

        // 2. SlidingWindowReference logic
        let mut sw = SlidingWindowReference::new(5_000, 5);
        assert_eq!(sw.count(), 0);

        // Insert 3 hits at t = 1000ms
        let (allowed, count) = sw.consume(1000, 1_000_000, "nonce1", 3);
        assert!(allowed);
        assert_eq!(count, 3);

        // Insert 2 hits at t = 2000ms (reaches limit 5)
        let (allowed, count) = sw.consume(2000, 2_000_000, "nonce2", 2);
        assert!(allowed);
        assert_eq!(count, 5);

        // Exceed limit
        let (allowed, count) = sw.consume(3000, 3_000_000, "nonce3", 1);
        assert!(!allowed);
        assert_eq!(count, 5);

        // Advance time past 1000ms window (> 6000ms): 3 hits at t=1000ms expire
        let (allowed, count) = sw.consume(6001, 6_001_000, "nonce4", 2);
        assert!(allowed);
        // Remaining 2 from t=2000ms + 2 new = 4
        assert_eq!(count, 4);

        // Max retention cap
        let mut sw_cap = SlidingWindowReference::new(10_000, 20);
        sw_cap.max_retention = 5;
        let (allowed, count) = sw_cap.consume(1000, 1_000_000, "n1", 4);
        assert!(allowed);
        assert_eq!(count, 4);
        let (allowed, count) = sw_cap.consume(2000, 2_000_000, "n2", 3);
        assert!(allowed);
        assert_eq!(count, 5); // Clamped at 5
        assert_eq!(sw_cap.count(), 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_token_bucket_certification_capacity_exhaustion() {
        use super::{HitOperation, TokenBucketReference};
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "tb_cert_exhaustion";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "burn",
                    "rate_limit": {{ "algorithm": "token_bucket", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy.match_entries(&[("action", "burn")]).unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("action", "burn")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        let mut ref_model = TokenBucketReference::new(10.0, 10_000);

        // 1. Consume 10 tokens in sequence (10 hits of cost 1)
        for i in 1..=10 {
            let decision = steward
                .check_limit(&key, &limit, HitOperation::Consume(1))
                .await
                .unwrap();
            let (ref_allowed, ref_observed) = ref_model.consume(0, 1);

            assert!(decision.allowed, "hit {i} must be allowed");
            assert!(ref_allowed, "reference hit {i} must be allowed");
            assert_eq!(
                decision.observed,
                (10 - i) as i64,
                "observed tokens must match"
            );
            assert_eq!(decision.observed, ref_observed);
        }

        // 2. Capacity exhaustion: 11th hit (capacity + 1) must be rejected
        let decision_exhausted = steward
            .check_limit(&key, &limit, HitOperation::Consume(1))
            .await
            .unwrap();
        let (ref_allowed_11, ref_observed_11) = ref_model.consume(0, 1);
        assert!(
            !decision_exhausted.allowed,
            "11th hit must be rejected when bucket is exhausted"
        );
        assert!(!ref_allowed_11);
        assert_eq!(decision_exhausted.observed, 0);
        assert_eq!(ref_observed_11, 0);

        // 3. Direct Redis state assertions
        let state: (Option<f64>, Option<i64>) = redis::cmd("HMGET")
            .arg(&key)
            .arg("tokens")
            .arg("timestamp_ms")
            .query(&mut conn)
            .unwrap();
        let stored_tokens = state.0.expect("tokens field must exist in Redis");
        let stored_ts = state.1.expect("timestamp_ms field must exist in Redis");
        assert!(
            stored_tokens < 0.05,
            "stored tokens in Redis must be ~0, got {stored_tokens}"
        );
        assert!(stored_ts > 0, "stored timestamp_ms must be positive");

        let pttl: i64 = redis::cmd("PTTL").arg(&key).query(&mut conn).unwrap();
        assert!(
            pttl > 0 && pttl <= 20_000,
            "PTTL must be positive and bounded by 2 * window_ms (20000ms), got {pttl}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_token_bucket_certification_fractional_refill_over_time() {
        use super::{HitOperation, TokenBucketReference};
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "tb_cert_refill";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "refill",
                    "rate_limit": {{ "algorithm": "token_bucket", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("action", "refill")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("action", "refill")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        let mut ref_model = TokenBucketReference::new(10.0, 1_000);

        // 1. Exhaust all 10 tokens in burst
        let d = steward
            .check_limit(&key, &limit, HitOperation::Consume(10))
            .await
            .unwrap();
        assert!(d.allowed);
        assert_eq!(d.observed, 0);

        // 2. Fetch authoritative Redis time
        let redis_time: (i64, i64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now_ms = (redis_time.0 * 1000) + (redis_time.1 / 1000);

        // 3. Simulate exactly 50% window elapsed (500ms on 1,000ms window)
        // With capacity 10 and window 1s, refill rate is 10 tokens/sec (0.01 tokens/ms).
        // 500ms elapsed refills exactly 5.0 tokens.
        let elapsed_50 = 500;
        let past_ts_50 = now_ms - elapsed_50;
        let _: () = redis::cmd("HSET")
            .arg(&key)
            .arg("tokens")
            .arg(0.0)
            .arg("timestamp_ms")
            .arg(past_ts_50)
            .query(&mut conn)
            .unwrap();
        ref_model.last_timestamp_ms = Some(past_ts_50 as u64);
        ref_model.tokens = 0.0;

        // Consume 5 tokens: must be allowed (refilled exactly 5 tokens)
        let d_50 = steward
            .check_limit(&key, &limit, HitOperation::Consume(5))
            .await
            .unwrap();
        let (ref_allowed_50, ref_obs_50) = ref_model.consume(now_ms as u64, 5);
        assert!(d_50.allowed, "50% window elapsed must allow 5 tokens");
        assert!(ref_allowed_50);
        assert_eq!(d_50.observed, 0);
        assert_eq!(ref_obs_50, 0);

        // Next 1 token immediately must be rejected (0 tokens left)
        let d_reject = steward
            .check_limit(&key, &limit, HitOperation::Consume(1))
            .await
            .unwrap();
        let (ref_allowed_rej, ref_obs_rej) = ref_model.consume(now_ms as u64, 1);
        assert!(!d_reject.allowed);
        assert!(!ref_allowed_rej);
        assert_eq!(d_reject.observed, 0);
        assert_eq!(ref_obs_rej, 0);

        // 4. Simulate another 30% window elapsed (300ms)
        let redis_time2: (i64, i64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now_ms2 = (redis_time2.0 * 1000) + (redis_time2.1 / 1000);
        let past_ts_30 = now_ms2 - 300;
        let _: () = redis::cmd("HSET")
            .arg(&key)
            .arg("tokens")
            .arg(0.0)
            .arg("timestamp_ms")
            .arg(past_ts_30)
            .query(&mut conn)
            .unwrap();
        ref_model.last_timestamp_ms = Some(past_ts_30 as u64);
        ref_model.tokens = 0.0;

        let d_30 = steward
            .check_limit(&key, &limit, HitOperation::Consume(3))
            .await
            .unwrap();
        let (ref_allowed_30, ref_obs_30) = ref_model.consume(now_ms2 as u64, 3);
        assert!(d_30.allowed, "30% window elapsed must allow 3 tokens");
        assert!(ref_allowed_30);
        assert_eq!(d_30.observed, 0);
        assert_eq!(ref_obs_30, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_token_bucket_certification_dynamic_capacity_update_live_keys() {
        use super::{HitOperation, TokenBucketReference};
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "tb_cert_dynamic_capacity";
        let config_initial = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "tier",
                    "value": "standard",
                    "rate_limit": {{ "algorithm": "token_bucket", "unit": "seconds", "requests_per_unit": 5 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_initial).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("tier", "standard")])
            .unwrap();
        let initial_limit = matched.rate_limits[0];
        let path = encode_canonical_path([("tier", "standard")]);
        let initial_key = rate_limit_key(domain, matched.policy_id, &path, &initial_limit);

        let mut ref_model = TokenBucketReference::new(5.0, 1_000);

        // 1. Exhaust initial capacity of 5
        let d1 = steward
            .check_limit(&initial_key, &initial_limit, HitOperation::Consume(5))
            .await
            .unwrap();
        assert!(d1.allowed);
        assert_eq!(d1.observed, 0);

        // Verify key exists in Redis
        let exists: bool = redis::cmd("EXISTS")
            .arg(&initial_key)
            .query(&mut conn)
            .unwrap();
        assert!(exists);

        // 2. Define updated limit with capacity 20 (same domain, policy ID, path, algorithm, unit)
        let updated_limit = crate::rate_limits::RateLimit {
            requests_per_unit: 20,
            unit: initial_limit.unit,
            algorithm: initial_limit.algorithm,
        };
        let updated_key = rate_limit_key(domain, matched.policy_id, &path, &updated_limit);

        // Counter identity is preserved across threshold changes!
        assert_eq!(
            initial_key, updated_key,
            "counter key identity must remain identical when capacity threshold changes"
        );

        // Update reference model capacity
        ref_model.update_capacity(20.0);
        assert_eq!(ref_model.capacity, 20.0);
        assert_eq!(ref_model.refill_per_ms, 0.02);

        // 3. Simulate 50% window elapsed (500ms on 1000ms window):
        // Under new capacity 20, 50% window refills 500 * 0.02 = 10.0 tokens!
        let redis_time: (i64, i64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now_ms = (redis_time.0 * 1000) + (redis_time.1 / 1000);
        let past_ts = now_ms - 500;
        let _: () = redis::cmd("HSET")
            .arg(&updated_key)
            .arg("tokens")
            .arg(0.0)
            .arg("timestamp_ms")
            .arg(past_ts)
            .query(&mut conn)
            .unwrap();
        ref_model.last_timestamp_ms = Some(past_ts as u64);
        ref_model.tokens = 0.0;

        // Under old capacity of 5, consuming 8 tokens would be impossible (capacity is only 5).
        // Under new capacity of 20, 10 tokens refilled >= 8 cost -> ALLOWED!
        let d_new = steward
            .check_limit(&updated_key, &updated_limit, HitOperation::Consume(8))
            .await
            .unwrap();
        let (ref_allowed, ref_observed) = ref_model.consume(now_ms as u64, 8);
        assert!(d_new.allowed, "new capacity of 20 must allow 8 tokens");
        assert!(ref_allowed);
        assert_eq!(d_new.observed, 2);
        assert_eq!(ref_observed, 2);

        // Direct Redis state assertions
        let tokens_in_redis: f64 = redis::cmd("HGET")
            .arg(&updated_key)
            .arg("tokens")
            .query(&mut conn)
            .unwrap();
        assert!(
            (tokens_in_redis - 2.0).abs() < 0.1,
            "Redis must hold ~2 tokens, got {tokens_in_redis}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_token_bucket_certification_clock_drift_and_backward_shift_protection() {
        use super::{HitOperation, TokenBucketReference};
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "tb_cert_clock_drift";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "clock_test",
                    "rate_limit": {{ "algorithm": "token_bucket", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("action", "clock_test")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("action", "clock_test")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        let mut ref_model = TokenBucketReference::new(10.0, 10_000);

        // Consume 7 tokens -> 3 tokens remaining
        let d1 = steward
            .check_limit(&key, &limit, HitOperation::Consume(7))
            .await
            .unwrap();
        let (ref_all_1, ref_obs_1) = ref_model.consume(0, 7);
        assert!(d1.allowed);
        assert!(ref_all_1);
        assert_eq!(d1.observed, 3);
        assert_eq!(ref_obs_1, 3);

        // Read Redis timestamp
        let redis_time: (i64, i64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now_ms = (redis_time.0 * 1000) + (redis_time.1 / 1000);

        // Simulate severe backward clock drift (e.g. Redis NTP step or failover clock shift 60s into future)
        let future_ts = now_ms + 60_000;
        let _: () = redis::cmd("HSET")
            .arg(&key)
            .arg("tokens")
            .arg(3.0)
            .arg("timestamp_ms")
            .arg(future_ts)
            .query(&mut conn)
            .unwrap();
        ref_model.last_timestamp_ms = Some(future_ts as u64);
        ref_model.tokens = 3.0;

        // Attempt consuming 4 tokens when Redis server time is still ~now_ms (< future_ts)
        // With clock drift protection, now_ms is clamped to future_ts, elapsed is 0, no refill happens!
        // 3 tokens < 4 cost -> must be rejected
        let d_reject = steward
            .check_limit(&key, &limit, HitOperation::Consume(4))
            .await
            .unwrap();
        let (ref_all_rej, ref_obs_rej) = ref_model.consume(now_ms as u64, 4);
        assert!(
            !d_reject.allowed,
            "4 tokens must be rejected when only 3 tokens remain"
        );
        assert!(!ref_all_rej);
        assert_eq!(d_reject.observed, 3);
        assert_eq!(ref_obs_rej, 3);

        // Consume 2 tokens: 3 tokens >= 2 cost -> allowed, leaves 1 token
        let d_allow = steward
            .check_limit(&key, &limit, HitOperation::Consume(2))
            .await
            .unwrap();
        let (ref_all_allow, ref_obs_allow) = ref_model.consume(now_ms as u64, 2);
        assert!(d_allow.allowed);
        assert!(ref_all_allow);
        assert_eq!(d_allow.observed, 1);
        assert_eq!(ref_obs_allow, 1);

        // Stored timestamp must NOT regress backwards to now_ms
        let state: (Option<f64>, Option<i64>) = redis::cmd("HMGET")
            .arg(&key)
            .arg("tokens")
            .arg("timestamp_ms")
            .query(&mut conn)
            .unwrap();
        assert!(
            state.1.unwrap() >= future_ts,
            "stored timestamp must remain clamped to future_ts"
        );
        assert!(
            (state.0.unwrap() - 1.0).abs() < 0.1,
            "stored tokens must be ~1"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_token_bucket_certification_direct_redis_state_and_refunds() {
        use super::{HitOperation, TokenBucketReference};
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "tb_cert_state_refunds";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "refund_test",
                    "rate_limit": {{ "algorithm": "token_bucket", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("action", "refund_test")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("action", "refund_test")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        let mut ref_model = TokenBucketReference::new(10.0, 10_000);

        // 1. Probe check: does not create key in Redis
        let probe = steward
            .check_limit(&key, &limit, HitOperation::Probe)
            .await
            .unwrap();
        assert!(probe.allowed);
        assert_eq!(probe.observed, 10);
        let exists_before: bool = redis::cmd("EXISTS").arg(&key).query(&mut conn).unwrap();
        assert!(!exists_before, "Probe check must not create key in Redis");

        // 2. Consume 8 tokens
        let d_consume = steward
            .check_limit(&key, &limit, HitOperation::Consume(8))
            .await
            .unwrap();
        let (ref_all, ref_obs) = ref_model.consume(0, 8);
        assert!(d_consume.allowed);
        assert!(ref_all);
        assert_eq!(d_consume.observed, 2);
        assert_eq!(ref_obs, 2);

        // Direct state assertions
        let key_type: String = redis::cmd("TYPE").arg(&key).query(&mut conn).unwrap();
        assert_eq!(key_type, "hash");

        let hexists_tokens: bool = redis::cmd("HEXISTS")
            .arg(&key)
            .arg("tokens")
            .query(&mut conn)
            .unwrap();
        let hexists_ts: bool = redis::cmd("HEXISTS")
            .arg(&key)
            .arg("timestamp_ms")
            .query(&mut conn)
            .unwrap();
        assert!(hexists_tokens);
        assert!(hexists_ts);

        let pttl: i64 = redis::cmd("PTTL").arg(&key).query(&mut conn).unwrap();
        assert!(pttl > 0 && pttl <= 20_000);

        // 3. Normal refund of 5 tokens: 2 + 5 = 7 tokens
        let d_refund = steward
            .check_limit(&key, &limit, HitOperation::Refund(5))
            .await
            .unwrap();
        let (ref_ref_all, ref_ref_obs) = ref_model.refund(0, 5);
        assert!(d_refund.allowed);
        assert!(ref_ref_all);
        assert_eq!(d_refund.observed, 7);
        assert_eq!(ref_ref_obs, 7);

        let tokens_after_refund: f64 = redis::cmd("HGET")
            .arg(&key)
            .arg("tokens")
            .query(&mut conn)
            .unwrap();
        assert!((tokens_after_refund - 7.0).abs() < 0.1);

        // 4. Excessive refund of 10 tokens: clamped to max capacity 10
        let d_excess_refund = steward
            .check_limit(&key, &limit, HitOperation::Refund(10))
            .await
            .unwrap();
        let (ref_ex_all, ref_ex_obs) = ref_model.refund(0, 10);
        assert!(d_excess_refund.allowed);
        assert!(ref_ex_all);
        assert_eq!(d_excess_refund.observed, 10);
        assert_eq!(ref_ex_obs, 10);

        let tokens_clamped: f64 = redis::cmd("HGET")
            .arg(&key)
            .arg("tokens")
            .query(&mut conn)
            .unwrap();
        assert!((tokens_clamped - 10.0).abs() < 0.1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sliding_window_certification_exact_log_boundary_expiry() {
        use super::{HitOperation, SlidingWindowReference};
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "sw_cert_boundary";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "boundary_test",
                    "rate_limit": {{ "algorithm": "sliding_window", "unit": "minutes", "requests_per_unit": 5 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("action", "boundary_test")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("action", "boundary_test")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        let window_ms = 60_000;
        let mut ref_model = SlidingWindowReference::new(window_ms, 5);

        // Get Redis server time
        let redis_time: (i64, i64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now_ms = (redis_time.0 * 1000) + (redis_time.1 / 1000);

        // Seed entries into Redis and reference model:
        // - ev1: now_ms - 70_000 (score < now_ms - 60_000 -> expired)
        // - ev2: now_ms - 60_000 (score == now_ms - 60_000 -> boundary, expired by ZREMRANGEBYSCORE)
        // - ev3: now_ms - 50_000 (score > now_ms - 60_000 -> valid inside window!)
        // - ev4: now_ms - 10_000 (valid inside window!)
        let entries = vec![
            (now_ms - 70_000, "ev_expired_1".to_string()),
            (now_ms - 60_000, "ev_expired_2".to_string()),
            (now_ms - 50_000, "ev_valid_3".to_string()),
            (now_ms - 10_000, "ev_valid_4".to_string()),
        ];

        for (score, member) in &entries {
            let _: () = redis::cmd("ZADD")
                .arg(&key)
                .arg(*score)
                .arg(member)
                .query(&mut conn)
                .unwrap();
        }
        ref_model.events = entries
            .iter()
            .map(|(score, m)| (*score as u64, m.clone()))
            .collect();

        assert_eq!(
            redis::cmd("ZCARD")
                .arg(&key)
                .query::<i64>(&mut conn)
                .unwrap(),
            4
        );

        // Consume 2 hits:
        // Expired events ev1 and ev2 must be pruned. Valid events ev3 and ev4 remain (2 events).
        // 2 remaining + 2 new hits = 4 <= 5 -> ALLOWED!
        let d1 = steward
            .check_limit(&key, &limit, HitOperation::Consume(2))
            .await
            .unwrap();
        assert!(d1.allowed, "2 hits must be allowed");
        assert_eq!(d1.observed, 4);

        // Verify Redis ZCARD is exactly 4
        let zcard1: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(zcard1, 4);

        // Verify ev1 and ev2 were evicted from Redis
        let s1: Option<f64> = redis::cmd("ZSCORE")
            .arg(&key)
            .arg("ev_expired_1")
            .query(&mut conn)
            .unwrap();
        let s2: Option<f64> = redis::cmd("ZSCORE")
            .arg(&key)
            .arg("ev_expired_2")
            .query(&mut conn)
            .unwrap();
        let s3: Option<f64> = redis::cmd("ZSCORE")
            .arg(&key)
            .arg("ev_valid_3")
            .query(&mut conn)
            .unwrap();
        assert!(s1.is_none(), "ev_expired_1 must be evicted");
        assert!(s2.is_none(), "ev_expired_2 must be evicted");
        assert!(s3.is_some(), "ev_valid_3 must be retained");

        // Now attempt consuming 2 more hits:
        // Current = 4. 4 + 2 = 6 > 5 -> REJECTED!
        let d2 = steward
            .check_limit(&key, &limit, HitOperation::Consume(2))
            .await
            .unwrap();
        assert!(
            !d2.allowed,
            "request for 2 hits when count=4 and limit=5 must be rejected"
        );
        assert_eq!(d2.observed, 4);

        // Cardinality in Redis must remain 4 (denied calls do not insert events)
        let zcard2: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(zcard2, 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sliding_window_certification_multi_replica_concurrency() {
        use super::HitOperation;
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "sw_cert_concurrency";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "endpoint",
                    "value": "concurrency_test",
                    "rate_limit": {{ "algorithm": "sliding_window", "unit": "seconds", "requests_per_unit": 100 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("endpoint", "concurrency_test")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("endpoint", "concurrency_test")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        // Two simulated replicas: replica_a and replica_b
        let replica_a = steward.clone();
        let replica_b = steward.clone();

        // 25 requests from replica_a with hits = 2 (50 hits total)
        // 25 requests from replica_b with hits = 2 (50 hits total)
        // Total = 100 hits, perfectly filling the limit of 100
        let mut handles = Vec::new();

        for _ in 0..25 {
            let rep_a = replica_a.clone();
            let k = key.clone();
            let lim = limit;
            handles.push(tokio::spawn(async move {
                rep_a.check_limit(&k, &lim, HitOperation::Consume(2)).await
            }));

            let rep_b = replica_b.clone();
            let k = key.clone();
            let lim = limit;
            handles.push(tokio::spawn(async move {
                rep_b.check_limit(&k, &lim, HitOperation::Consume(2)).await
            }));
        }

        // Await all 50 concurrent requests
        for handle in handles {
            let res = handle.await.unwrap().unwrap();
            assert!(res.allowed, "concurrent hit must be allowed");
        }

        // Direct Redis state assertions:
        // 1. Exact ZCARD matches sum of all hits: 50 requests * 2 hits = 100
        let card: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(card, 100, "ZCARD in Redis must exactly equal 100");

        // 2. Cardinality and membership: all 100 members must be completely unique with ZERO collisions
        let members: Vec<String> = redis::cmd("ZRANGE")
            .arg(&key)
            .arg(0)
            .arg(-1)
            .query(&mut conn)
            .unwrap();
        assert_eq!(members.len(), 100);

        let unique_members: std::collections::HashSet<_> = members.iter().cloned().collect();
        assert_eq!(
            unique_members.len(),
            100,
            "zero collisions across multi-replica concurrent writes"
        );

        // Extract nonces: exactly 50 distinct nonces (one per request)
        let mut nonces = std::collections::HashSet::new();
        for m in &members {
            let parts: Vec<&str> = m.split(':').collect();
            assert_eq!(parts.len(), 3, "member format must be <usec>:<nonce>:<i>");
            assert!(parts[0].parse::<u64>().is_ok());
            assert_eq!(parts[1].len(), 32);
            let idx: u64 = parts[2].parse().unwrap();
            assert!(idx == 1 || idx == 2);
            nonces.insert(parts[1].to_string());
        }
        assert_eq!(
            nonces.len(),
            50,
            "must have exactly 50 unique nonces across 50 requests"
        );

        // 3. Limit is now fully exhausted. Any subsequent request must be denied.
        let d_over = replica_a
            .check_limit(&key, &limit, HitOperation::Consume(1))
            .await
            .unwrap();
        assert!(
            !d_over.allowed,
            "request beyond limit of 100 must be denied"
        );
        assert_eq!(d_over.observed, 100);

        let final_card: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(final_card, 100, "denied request must not increment ZCARD");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sliding_window_certification_cardinality_and_membership_format() {
        use super::{HitOperation, SlidingWindowReference};
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "sw_cert_format";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "format_test",
                    "rate_limit": {{ "algorithm": "sliding_window", "unit": "minutes", "requests_per_unit": 50 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("action", "format_test")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("action", "format_test")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        let mut ref_model = SlidingWindowReference::new(60_000, 50);

        // Check varying hits: 1, 3, 5, 2 (sum = 11 hits)
        let hit_costs = vec![1, 3, 5, 2];
        let mut running_sum = 0;
        for hits in hit_costs {
            let decision = steward
                .check_limit(&key, &limit, HitOperation::Consume(hits))
                .await
                .unwrap();
            running_sum += hits;
            assert!(decision.allowed);
            assert_eq!(decision.observed, running_sum as i64);

            let (ref_all, ref_count) = ref_model.consume(0, 0, &format!("nonce_{hits}"), hits);
            assert!(ref_all);
            assert_eq!(ref_count, running_sum as i64);
        }

        // Direct Redis assertions:
        // 1. ZCARD matches reference count: exactly 11
        let card: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(card, 11);
        assert_eq!(card as usize, ref_model.count());

        // 2. Inspect members with scores
        let members_with_scores: Vec<(String, f64)> = redis::cmd("ZRANGE")
            .arg(&key)
            .arg(0)
            .arg(-1)
            .arg("WITHSCORES")
            .query(&mut conn)
            .unwrap();
        assert_eq!(members_with_scores.len(), 11);

        let redis_time: (i64, i64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now_ms = ((redis_time.0 * 1000) + (redis_time.1 / 1000)) as f64;

        for (member, score) in members_with_scores {
            // Score must match Redis TIME in milliseconds (within 5 seconds)
            assert!(
                (now_ms - score).abs() < 5000.0,
                "score {score} must match Redis TIME ~{now_ms}"
            );

            // Member format: <usec>:<nonce>:<i>
            let parts: Vec<&str> = member.split(':').collect();
            assert_eq!(
                parts.len(),
                3,
                "member {member} must have 3 colon-separated parts"
            );

            let usec: u64 = parts[0].parse().expect("part 0 must be u64 usec timestamp");
            assert!(usec > 0);

            let nonce = parts[1];
            assert_eq!(nonce.len(), 32, "part 1 must be 32-hex-char nonce");
            assert!(
                nonce.chars().all(|c| c.is_ascii_hexdigit()),
                "nonce must be valid hex"
            );

            let idx: u64 = parts[2].parse().expect("part 2 must be u64 hit index");
            assert!(
                (1..=5).contains(&idx),
                "hit index must be between 1 and max hits (5)"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sliding_window_certification_10000_event_retention_cap() {
        use super::{HitOperation, SlidingWindowReference};
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "sw_cert_retention_cap";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "cap_test",
                    "rate_limit": {{ "algorithm": "sliding_window", "unit": "minutes", "requests_per_unit": 25000 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("action", "cap_test")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("action", "cap_test")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        let mut ref_model = SlidingWindowReference::new(60_000, 25_000);

        // Fetch current Redis time
        let redis_time: (i64, i64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now_ms = (redis_time.0 * 1000) + (redis_time.1 / 1000);
        let seed_score = now_ms - 2000;

        // Seed 10,000 items in Redis
        let mut cmd = redis::cmd("ZADD");
        cmd.arg(&key);
        for i in 0..10_000 {
            cmd.arg(seed_score).arg(format!("seed_cert:{i:05}"));
            ref_model
                .events
                .push((seed_score as u64, format!("seed_cert:{i:05}")));
        }
        let _: () = cmd.query(&mut conn).unwrap();
        assert_eq!(
            redis::cmd("ZCARD")
                .arg(&key)
                .query::<i64>(&mut conn)
                .unwrap(),
            10_000
        );
        assert_eq!(ref_model.count(), 10_000);

        // Add 50 hits via sliding window script (limit is 25,000, so allowed)
        // Set will temporarily reach 10,050 then be clamped to exactly 10,000 by ZREMRANGEBYRANK
        let decision = steward
            .check_limit(&key, &limit, HitOperation::Consume(50))
            .await
            .unwrap();
        let (ref_all, ref_observed) = ref_model.consume(now_ms as u64, 0, "nonce_cap", 50);

        assert!(decision.allowed);
        assert_eq!(decision.observed, 10_000);
        assert!(ref_all);
        assert_eq!(ref_observed, 10_000);

        // Redis cardinality must be strictly clamped to 10,000
        let card: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(card, 10_000);
        assert_eq!(ref_model.count(), 10_000);

        // Verify oldest 50 items (seed_cert:00000 to seed_cert:00049) were evicted
        for i in 0..50 {
            let score: Option<f64> = redis::cmd("ZSCORE")
                .arg(&key)
                .arg(format!("seed_cert:{i:05}"))
                .query(&mut conn)
                .unwrap();
            assert!(
                score.is_none(),
                "seed_cert:{i:05} must be evicted by rank cap"
            );
        }

        // Verify item 50 was retained
        let score_50: Option<f64> = redis::cmd("ZSCORE")
            .arg(&key)
            .arg("seed_cert:00050")
            .query(&mut conn)
            .unwrap();
        assert!(score_50.is_some(), "seed_cert:00050 must be retained");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sliding_window_certification_direct_redis_state_and_refunds() {
        use super::HitOperation;
        use crate::rate_limits::{encode_canonical_path, rate_limit_key};

        let domain = "sw_cert_state_refunds";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "action",
                    "value": "state_test",
                    "rate_limit": {{ "algorithm": "sliding_window", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection().unwrap();
        let active_cfg = steward.active_config();
        let domain_policy = active_cfg.domains.get(domain).unwrap();
        let matched = domain_policy
            .match_entries(&[("action", "state_test")])
            .unwrap();
        let limit = matched.rate_limits[0];
        let path = encode_canonical_path([("action", "state_test")]);
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit);

        // 1. Probe check: does not create key in Redis
        let probe = steward
            .check_limit(&key, &limit, HitOperation::Probe)
            .await
            .unwrap();
        assert!(probe.allowed);
        assert_eq!(probe.observed, 0);

        let exists_before: bool = redis::cmd("EXISTS").arg(&key).query(&mut conn).unwrap();
        assert!(!exists_before, "Probe check must not create key in Redis");

        // 2. Consume 6 hits
        let d = steward
            .check_limit(&key, &limit, HitOperation::Consume(6))
            .await
            .unwrap();
        assert!(d.allowed);
        assert_eq!(d.observed, 6);

        // Direct Redis assertions:
        let exists: bool = redis::cmd("EXISTS").arg(&key).query(&mut conn).unwrap();
        assert!(exists);

        let key_type: String = redis::cmd("TYPE").arg(&key).query(&mut conn).unwrap();
        assert_eq!(key_type, "zset");

        let card: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(card, 6);

        let pttl: i64 = redis::cmd("PTTL").arg(&key).query(&mut conn).unwrap();
        assert!(pttl > 0 && pttl <= 20_000);

        // 3. Attempt refund: refunds are non-refundable / unsupported for sliding window
        let refund_res = steward
            .check_limit(&key, &limit, HitOperation::Refund(2))
            .await;
        assert!(
            refund_res.is_err(),
            "refunds must be rejected for sliding window"
        );
        let err_msg = refund_res.unwrap_err().to_string();
        assert!(
            err_msg.contains("refunds are unsupported for sliding window"),
            "error message was: {err_msg}"
        );

        // ZCARD in Redis must remain strictly 6
        let card_after: i64 = redis::cmd("ZCARD").arg(&key).query(&mut conn).unwrap();
        assert_eq!(card_after, 6);
    }

    #[tokio::test]
    async fn test_concurrent_multi_rule_evaluation_and_ordering() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "domain_concurrent_eval";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "tier",
                    "value": "gold",
                    "rate_limit": {{ "unit": "seconds", "requests_per_unit": 5 }}
                }},
                {{
                    "key": "tier",
                    "value": "silver",
                    "rate_limit": {{ "unit": "seconds", "requests_per_unit": 10 }}
                }},
                {{
                    "key": "tier",
                    "value": "bronze",
                    "rate_limit": {{ "unit": "seconds", "requests_per_unit": 1 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection_manager().await.unwrap();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut conn).await.unwrap();

        // Send a request with 4 descriptors: gold, bronze (consume 2 hits -> will be over limit), silver, unmatched
        let request = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "tier".to_string(),
                        value: "gold".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "tier".to_string(),
                        value: "bronze".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(2), // exceeds limit of 1
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "tier".to_string(),
                        value: "silver".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "unmatched".to_string(),
                        value: "none".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
            ],
            hits_addend: 0,
        };

        let response = steward
            .should_rate_limit(tonic::Request::new(request))
            .await
            .unwrap()
            .into_inner();

        // Strict 1:1 descriptor ordering:
        assert_eq!(response.statuses.len(), 4);
        assert_eq!(response.statuses[0].code, Code::Ok as i32); // gold: 1/5 ok
        assert_eq!(response.statuses[1].code, Code::OverLimit as i32); // bronze: 2/1 over limit
        assert_eq!(response.statuses[2].code, Code::Ok as i32); // silver: 1/10 ok
        assert_eq!(response.statuses[3].code, Code::Ok as i32); // unmatched: unconstrained ok
        assert_eq!(response.statuses[3].current_limit, None);

        // F06 Precedence: Bronze was over limit -> overall code must be OVER_LIMIT
        assert_eq!(response.overall_code, Code::OverLimit as i32);
    }

    #[tokio::test]
    async fn test_noscript_cache_recovery_after_script_flush() {
        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let domain = "domain_noscript_recovery";
        let config_json = format!(
            r#"{{
            "domain": "{domain}",
            "descriptors": [
                {{
                    "key": "algo",
                    "value": "fixed",
                    "rate_limit": {{ "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 10 }}
                }},
                {{
                    "key": "algo",
                    "value": "token",
                    "rate_limit": {{ "algorithm": "token_bucket", "unit": "seconds", "requests_per_unit": 10 }}
                }},
                {{
                    "key": "algo",
                    "value": "sliding",
                    "rate_limit": {{ "algorithm": "sliding_window", "unit": "seconds", "requests_per_unit": 10 }}
                }}
            ]
        }}"#
        );

        let Some((_server, steward, client)) = setup_test_steward(&config_json).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let mut conn = client.get_connection_manager().await.unwrap();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut conn).await.unwrap();

        // Step 1: Initial request to warm up and load scripts into Redis
        let warm_req = RateLimitRequest {
            domain: domain.to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "algo".to_string(),
                        value: "fixed".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "algo".to_string(),
                        value: "token".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "algo".to_string(),
                        value: "sliding".to_string(),
                    }],
                    limit: None,
                    hits_addend: Some(1),
                    is_negative_hits: false,
                },
            ],
            hits_addend: 0,
        };

        let res1 = steward
            .should_rate_limit(tonic::Request::new(warm_req.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(res1.overall_code, Code::Ok as i32);
        assert_eq!(res1.statuses.len(), 3);

        // Step 2: Flush all cached Lua scripts from Redis (simulating Redis restart / script cache flush)
        let _: () = redis::cmd("SCRIPT")
            .arg("FLUSH")
            .query_async(&mut conn)
            .await
            .unwrap();

        // Step 3: Immediately send subsequent concurrent requests across all algorithms
        // Steward must catch NOSCRIPT, reload scripts into Redis, and succeed without errors
        let res2 = steward
            .should_rate_limit(tonic::Request::new(warm_req))
            .await
            .expect("RateLimit request must succeed after SCRIPT FLUSH via transparent recovery")
            .into_inner();

        assert_eq!(res2.overall_code, Code::Ok as i32);
        assert_eq!(res2.statuses.len(), 3);
        assert_eq!(res2.statuses[0].code, Code::Ok as i32);
        assert_eq!(res2.statuses[1].code, Code::Ok as i32);
        assert_eq!(res2.statuses[2].code, Code::Ok as i32);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_telemetry_metrics_allowed_and_denied_flow() {
        let Some((_server, steward, _client)) = setup_test_steward(r#"{
            "domain": "telemetry_test",
            "descriptors": [
                {
                    "key": "action",
                    "value": "login",
                    "rate_limit": { "algorithm": "fixed_window", "unit": "seconds", "requests_per_unit": 1 }
                }
            ]
        }"#).await else {
            eprintln!("Skipping test: redis-server not available");
            return;
        };

        let (rx, sink) = cadence::SpyMetricSink::new();
        let statsd_client = std::sync::Arc::new(cadence::StatsdClient::from_sink("steward", sink));
        let steward = steward.with_metrics(statsd_client);

        use crate::proto::envoy::extensions::common::ratelimit::v3::{
            RateLimitDescriptor, rate_limit_descriptor::Entry,
        };
        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let req = RateLimitRequest {
            domain: "telemetry_test".to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "action".to_string(),
                    value: "login".to_string(),
                }],
                limit: None,
                hits_addend: Some(1),
                is_negative_hits: false,
            }],
            hits_addend: 0,
        };

        // First request is allowed
        let res1 = steward
            .should_rate_limit(tonic::Request::new(req.clone()))
            .await
            .unwrap();
        assert_eq!(res1.into_inner().overall_code, Code::Ok as i32);

        // Collect emitted metrics
        let mut metrics_received = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            metrics_received.push(String::from_utf8_lossy(&msg).to_string());
        }

        let combined = metrics_received.join("\n");
        assert!(
            combined.contains("steward.requests.total:1|c"),
            "must emit requests.total"
        );
        assert!(
            combined.contains("steward.requests.allowed:1|c"),
            "must emit requests.allowed"
        );
        assert!(
            combined.contains("steward.rpc.duration.allowed:"),
            "must emit rpc.duration.allowed timer"
        );
        assert!(
            combined.contains("steward.rpc.duration:"),
            "must emit rpc.duration timer"
        );
        assert!(
            combined.contains("steward.admission.wait_time:"),
            "must emit admission.wait_time timer"
        );
        assert!(
            combined.contains("steward.redis.duration:"),
            "must emit redis.duration timer"
        );
        assert!(
            combined.contains("steward.in_flight_requests:"),
            "must emit in_flight_requests gauge"
        );
        assert!(
            combined.contains("steward.config.age_seconds:"),
            "must emit config.age_seconds gauge"
        );
        assert!(
            combined.contains("steward.config.version:"),
            "must emit config.version gauge"
        );

        // Second request is over limit
        let res2 = steward
            .should_rate_limit(tonic::Request::new(req))
            .await
            .unwrap();
        assert_eq!(res2.into_inner().overall_code, Code::OverLimit as i32);

        let mut metrics_received2 = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            metrics_received2.push(String::from_utf8_lossy(&msg).to_string());
        }

        let combined2 = metrics_received2.join("\n");
        assert!(
            combined2.contains("steward.requests.over_limit:1|c"),
            "must emit requests.over_limit"
        );
        assert!(
            combined2.contains("steward.rpc.duration.denied:"),
            "must emit rpc.duration.denied timer"
        );
        assert!(
            combined2.contains("steward.rpc.duration:"),
            "must emit rpc.duration timer"
        );
    }

    #[tokio::test]
    async fn test_telemetry_metrics_on_admission_load_shedding() {
        let raw: crate::config_source::RawRateLimitsConfig = serde_json::from_str(
            r#"{
            "domain": "test",
            "descriptors": [
                {
                    "key": "a",
                    "value": "b",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#,
        )
        .unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx_conf) = tokio::sync::watch::channel(compiled);

        let (rx, sink) = cadence::SpyMetricSink::new();
        let statsd_client = std::sync::Arc::new(cadence::StatsdClient::from_sink("steward", sink));

        let steward = super::Steward::for_test(rx_conf)
            .await
            .with_metrics(statsd_client)
            .with_max_concurrent_requests(0); // 0 permits -> load shedding

        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        let req = RateLimitRequest {
            domain: "test".to_string(),
            descriptors: vec![],
            hits_addend: 0,
        };

        let err = steward
            .should_rate_limit(tonic::Request::new(req))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);

        let mut metrics_received = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            metrics_received.push(String::from_utf8_lossy(&msg).to_string());
        }
        let combined = metrics_received.join("\n");
        assert!(combined.contains("steward.requests.total:1|c"));
        assert!(combined.contains("steward.requests.rejected_admission:1|c"));
        assert!(combined.contains("steward.admission.wait_time:"));
        assert!(combined.contains("steward.rpc.duration:"));
    }

    #[tokio::test]
    async fn test_in_flight_gauge_lifecycle() {
        let raw: crate::config_source::RawRateLimitsConfig = serde_json::from_str(
            r#"{
            "domain": "configured_domain",
            "descriptors": [
                {
                    "key": "a",
                    "value": "b",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#,
        )
        .unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx_conf) = tokio::sync::watch::channel(compiled);

        let (rx, sink) = cadence::SpyMetricSink::new();
        let statsd_client = std::sync::Arc::new(cadence::StatsdClient::from_sink("steward", sink));

        let steward = super::Steward::for_test(rx_conf)
            .await
            .with_metrics(statsd_client);

        use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;

        assert_eq!(steward.in_flight_requests(), 0);

        let req = RateLimitRequest {
            domain: "unconfigured_domain".to_string(),
            descriptors: vec![],
            hits_addend: 0,
        };

        let _ = steward.should_rate_limit(tonic::Request::new(req)).await;

        // In-flight should return to 0
        assert_eq!(steward.in_flight_requests(), 0);

        let mut metrics_received = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            metrics_received.push(String::from_utf8_lossy(&msg).to_string());
        }
        let combined = metrics_received.join("\n");
        assert!(
            combined.contains("steward.in_flight_requests:1|g"),
            "must track 1 in-flight during request"
        );
        assert!(
            combined.contains("steward.in_flight_requests:0|g"),
            "must restore 0 in-flight after request completion"
        );
    }

    #[test]
    fn test_redis_url_normalization_and_sanitization() {
        use super::{normalize_redis_url, sanitize_url};

        // 1. Bare host gets redis:// prefix
        assert_eq!(
            normalize_redis_url("127.0.0.1:6379").unwrap(),
            "redis://127.0.0.1:6379"
        );

        // 2. redis:// preserved
        assert_eq!(
            normalize_redis_url("redis://my-host:6379/1").unwrap(),
            "redis://my-host:6379/1"
        );

        // 3. rediss:// with TLS preserved
        assert_eq!(
            normalize_redis_url("rediss://user:secret@redis-cluster.example.com:6380/0").unwrap(),
            "rediss://user:secret@redis-cluster.example.com:6380/0"
        );

        // 4. sanitize_url redacts passwords
        let sanitized =
            sanitize_url("rediss://appuser:supersecretpassword@secure-redis.cloud:6380/2");
        assert!(!sanitized.contains("supersecretpassword"));
        assert!(sanitized.contains("*****"));
        assert_eq!(
            sanitized,
            "rediss://appuser:*****@secure-redis.cloud:6380/2"
        );

        // 5. sanitize_url handles URLs without passwords cleanly
        assert_eq!(
            sanitize_url("redis://localhost:6379/0"),
            "redis://localhost:6379/0"
        );

        // 6. Empty target errors
        assert!(normalize_redis_url("   ").is_err());
    }

    #[tokio::test]
    async fn test_grpc_health_service_lifecycle_and_shutdown_drain() {
        use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitServiceServer;
        use tonic::transport::Server;
        use tonic_health::pb::HealthCheckRequest;
        use tonic_health::pb::health_check_response::ServingStatus;
        use tonic_health::pb::health_client::HealthClient;

        let json_str = r#"{"domain": "default", "descriptors": [{"key": "k", "rate_limit": {"unit": "seconds", "requests_per_unit": 10}}]}"#;
        let raw: crate::config_source::RawRateLimitsConfig =
            serde_json::from_str(json_str).unwrap();
        let compiled = crate::config_source::compile_rate_limits(raw).unwrap();
        let (_tx, rx_conf) = tokio::sync::watch::channel(compiled);
        let steward = super::Steward::for_test(rx_conf).await;

        let (health_reporter, health_service) = tonic_health::server::health_reporter();
        health_reporter
            .set_serving::<RateLimitServiceServer<super::Steward>>()
            .await;
        health_reporter
            .set_service_status("", tonic_health::ServingStatus::Serving)
            .await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let server_handle = tokio::spawn(async move {
            Server::builder()
                .add_service(health_service)
                .add_service(RateLimitServiceServer::new(steward))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let channel = tonic::transport::Channel::from_shared(format!("http://{local_addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();

        let mut health_client = HealthClient::new(channel);

        // 1. Initial status for overall service ("") is SERVING
        let resp_overall = health_client
            .check(HealthCheckRequest {
                service: "".to_string(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp_overall.status, ServingStatus::Serving as i32);

        // 2. Initial status for RateLimitService is SERVING
        let rls_service_name = "envoy.service.ratelimit.v3.RateLimitService";
        let resp_rls = health_client
            .check(HealthCheckRequest {
                service: rls_service_name.to_string(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp_rls.status, ServingStatus::Serving as i32);

        // 3. Status for unknown service is NotFound error
        let err_unknown = health_client
            .check(HealthCheckRequest {
                service: "unknown.service.Name".to_string(),
            })
            .await;
        assert!(err_unknown.is_err());
        assert_eq!(err_unknown.unwrap_err().code(), tonic::Code::NotFound);

        // 4. Mark NOT_SERVING (simulating SIGTERM initiation)
        health_reporter
            .set_not_serving::<RateLimitServiceServer<super::Steward>>()
            .await;
        health_reporter
            .set_service_status("", tonic_health::ServingStatus::NotServing)
            .await;

        let resp_not_serving = health_client
            .check(HealthCheckRequest {
                service: rls_service_name.to_string(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp_not_serving.status, ServingStatus::NotServing as i32);

        // 5. Trigger graceful shutdown and await termination
        let _ = shutdown_tx.send(());
        let server_res =
            tokio::time::timeout(std::time::Duration::from_secs(3), server_handle).await;
        assert!(
            server_res.is_ok(),
            "server must terminate within shutdown timeout"
        );
    }
}
