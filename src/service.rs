use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cadence::{NopMetricSink, StatsdClient};
use redis::Script;
use redis::aio::ConnectionManager;
use tokio::sync::watch::Receiver;
use tonic::Response;
use tracing::{debug, error, info, warn};

use crate::metrics::{SharedMetrics, count, gauge, time};
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
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes)
        .expect("system randomness failed to generate nonce");
    let mut hex = String::with_capacity(32);
    for b in bytes {
        let _ = std::fmt::write(&mut hex, format_args!("{:02x}", b));
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

struct DescriptorMatch {
    limits_to_check: Vec<(String, RateLimit)>,
    operation: HitOperation,
}

enum DescriptorEvaluation {
    Unmatched,
    Matched(DescriptorMatch),
}

#[derive(Clone)]
pub struct Steward {
    config_rx: Receiver<RateLimitConfigs>,
    redis: ConnectionManager,
    default_ttl: usize,
    metrics: SharedMetrics,
    scripts: StewardScripts,
    pub execution_timeout: Duration,
    pub admission_semaphore: Arc<tokio::sync::Semaphore>,
}

impl Steward {
    /// Construct a service with metrics disabled.
    pub async fn new(
        redis_host: &str,
        default_ttl: usize,
        config_rx: Receiver<RateLimitConfigs>,
    ) -> Self {
        Self::try_new(
            redis_host,
            default_ttl,
            config_rx,
            std::sync::Arc::new(StatsdClient::from_sink("", NopMetricSink)),
        )
        .await
        .expect("failed to create Steward service")
    }

    /// Construct a service and return configuration or connection manager initialization errors.
    pub async fn try_new(
        redis_host: &str,
        default_ttl: usize,
        config_rx: Receiver<RateLimitConfigs>,
        metrics: SharedMetrics,
    ) -> Result<Self, String> {
        let client = redis::Client::open(format!("redis://{redis_host}"))
            .map_err(|error| format!("invalid Redis configuration: {error}"))?;
        let redis = ConnectionManager::new(client)
            .await
            .map_err(|error| format!("failed to create Redis connection manager: {error}"))?;

        Ok(Self {
            config_rx,
            redis,
            default_ttl,
            metrics,
            scripts: StewardScripts::default(),
            execution_timeout: DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
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
            default_ttl: 10,
            metrics: Arc::new(StatsdClient::from_sink("", NopMetricSink)),
            scripts: StewardScripts::default(),
            execution_timeout: DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
        }
    }

    pub async fn check_limit(
        &self,
        key: &str,
        limit: &RateLimit,
        op: HitOperation,
    ) -> redis::RedisResult<Decision> {
        let mut connection = self.redis.clone();
        let window_seconds = limit
            .unit
            .seconds()
            .unwrap_or(self.default_ttl as u64)
            .max(1);

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
                    let current: i64 = self
                        .scripts
                        .fixed_window_refund
                        .key(key)
                        .arg(hits as i64)
                        .invoke_async(&mut connection)
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
                    let outcome: ScriptOutcome = self
                        .scripts
                        .token_bucket_refund
                        .key(key)
                        .arg(capacity)
                        .arg(refill_per_ms)
                        .arg(hits as i64)
                        .arg(window_ms)
                        .invoke_async(&mut connection)
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
                        let current: i64 = self
                            .scripts
                            .fixed_window
                            .key(key)
                            .arg(hits)
                            .arg(window_seconds)
                            .invoke_async(&mut connection)
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
                        let outcome: ScriptOutcome = self
                            .scripts
                            .token_bucket
                            .key(key)
                            .arg(capacity)
                            .arg(refill_per_ms)
                            .arg(hits)
                            .arg(window_ms)
                            .invoke_async(&mut connection)
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
                        let outcome: ScriptOutcome = self
                            .scripts
                            .sliding_window
                            .key(key)
                            .arg(window_ms)
                            .arg(limit.requests_per_unit)
                            .arg(hits)
                            .arg(nonce)
                            .invoke_async(&mut connection)
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
            Ok(decision) => {
                gauge(
                    &self.metrics,
                    "rate_limit.observed",
                    decision.observed.max(0) as u64,
                );
            }
            Err(error) => {
                error!(rate_limit_key = key, %error, "failed to update rate limit in Redis");
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

pub fn duration_until_reset_for(limit: &RateLimit, default_ttl: usize) -> u64 {
    let window_secs = limit.unit.seconds().unwrap_or(default_ttl as u64).max(1);
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
    default_ttl: usize,
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
            let reset1 = duration_until_reset_for(l1, default_ttl);
            let reset2 = duration_until_reset_for(l2, default_ttl);
            let unit_secs1 = l1.unit.seconds().unwrap_or(default_ttl as u64);
            let unit_secs2 = l2.unit.seconds().unwrap_or(default_ttl as u64);

            reset2
                .cmp(&reset1)
                .then_with(|| unit_secs1.cmp(&unit_secs2))
        });

        let (gov_limit, gov_decision) = violated[0];
        let remaining = limit_remaining_for(gov_limit, gov_decision);
        let reset_secs = duration_until_reset_for(gov_limit, default_ttl);

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
            let unit_secs1 = l1.unit.seconds().unwrap_or(default_ttl as u64);
            let unit_secs2 = l2.unit.seconds().unwrap_or(default_ttl as u64);

            ratio_cmp
                .then_with(|| rem1.cmp(&rem2))
                .then_with(|| unit_secs1.cmp(&unit_secs2))
        });

        let (gov_limit, gov_decision) = allowed[0];
        let remaining = limit_remaining_for(gov_limit, gov_decision);
        let reset_secs = duration_until_reset_for(gov_limit, default_ttl);

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

pub fn is_trusted_caller(metadata: &tonic::metadata::MetadataMap) -> bool {
    metadata
        .get("x-steward-trusted")
        .and_then(|v| v.to_str().ok())
        == Some("true")
        || metadata
            .get("x-steward-internal")
            .and_then(|v| v.to_str().ok())
            == Some("true")
        || metadata
            .get("x-trusted-caller")
            .and_then(|v| v.to_str().ok())
            == Some("true")
        || metadata.contains_key("x-forwarded-client-cert")
        || metadata.contains_key("authorization")
}

#[tonic::async_trait]
impl RateLimitService for Steward {
    async fn should_rate_limit(
        &self,
        request: tonic::Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, tonic::Status> {
        count(&self.metrics, "requests.total", 1);

        // Global Admission Control (Load Shedding): Immediate non-blocking permit acquisition
        let _permit = match self.admission_semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                count(&self.metrics, "requests.rejected_admission", 1);
                return Err(tonic::Status::resource_exhausted(
                    "admission limit reached; request rejected due to load shedding",
                ));
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(tonic::Status::unavailable("service is shutting down"));
            }
        };

        let (metadata, _, request) = request.into_parts();

        // 1. Request Dimension & Override Validation
        if let Err(status) = validate_request(&request) {
            count(&self.metrics, "requests.invalid", 1);
            return Err(status);
        }

        // 2. Caller Authorization for Negative Hits (Refunds)
        let has_negative_hits = request.descriptors.iter().any(|d| d.is_negative_hits);
        if has_negative_hits && !is_trusted_caller(&metadata) {
            count(&self.metrics, "requests.unauthorized_refund", 1);
            return Err(tonic::Status::permission_denied(
                "untrusted caller cannot perform negative hits (refund)",
            ));
        }

        // 3. In-memory Hierarchical Match Phase (precompiled trie lookup)
        let evaluations: Vec<DescriptorEvaluation> = {
            let configs = self.config_rx.borrow();
            let Some(domain_policy) = configs.get(&request.domain) else {
                count(&self.metrics, "requests.unconfigured", 1);
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
                return Ok(Response::new(build_response(false, statuses)));
            };

            let mut evals = Vec::with_capacity(request.descriptors.len());
            for req_desc in &request.descriptors {
                let hit_cost = compute_descriptor_hit_cost(req_desc, request.hits_addend)?;
                let op = if req_desc.is_negative_hits {
                    HitOperation::Refund(hit_cost)
                } else if hit_cost == 0 {
                    HitOperation::Probe
                } else {
                    HitOperation::Consume(hit_cost)
                };

                let entries: Vec<(&str, &str)> = req_desc
                    .entries
                    .iter()
                    .map(|e| (e.key.as_str(), e.value.as_str()))
                    .collect();

                let match_result = domain_policy.match_entries(&entries);
                match match_result {
                    Some(res) => {
                        if req_desc.is_negative_hits
                            && res
                                .rate_limits
                                .iter()
                                .any(|l| l.algorithm == Algorithm::SlidingWindow)
                        {
                            return Err(tonic::Status::failed_precondition(
                                "refunds are unsupported for sliding-window rate limits",
                            ));
                        }

                        let encoded_path = encode_canonical_path(entries);
                        let override_ = req_desc.limit.as_ref();
                        let mut limits_to_check = Vec::new();

                        for configured_limit in res.rate_limits {
                            let effective_limit = override_
                                .map(|o| configured_limit.with_override(o))
                                .unwrap_or_else(|| *configured_limit);

                            // Finding F10: Counter key identity is decoupled from mutable requests_per_unit capacity
                            let key = rate_limit_key(
                                &request.domain,
                                res.policy_id,
                                &encoded_path,
                                configured_limit,
                                self.default_ttl,
                            );
                            limits_to_check.push((key, effective_limit));
                        }

                        if limits_to_check.is_empty() {
                            evals.push(DescriptorEvaluation::Unmatched);
                        } else {
                            evals.push(DescriptorEvaluation::Matched(DescriptorMatch {
                                limits_to_check,
                                operation: op,
                            }));
                        }
                    }
                    None => {
                        evals.push(DescriptorEvaluation::Unmatched);
                    }
                }
            }
            evals
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
        let default_ttl = self.default_ttl;
        let eval_result = tokio::time::timeout(effective_timeout, async {
            let mut statuses = Vec::with_capacity(evaluations.len());
            let mut any_rule_over_limit = false;
            let mut first_redis_error = None;

            for eval in evaluations {
                match eval {
                    DescriptorEvaluation::Unmatched => {
                        count(&self.metrics, "descriptors.unmatched", 1);
                        statuses.push(DescriptorStatus {
                            code: Code::Ok as i32,
                            current_limit: None,
                            limit_remaining: 0,
                            duration_until_reset: None,
                            quota: None,
                        });
                    }
                    DescriptorEvaluation::Matched(desc_match) => {
                        let mut successful_decisions =
                            Vec::with_capacity(desc_match.limits_to_check.len());
                        let mut failed_rules = Vec::new();

                        for (key, limit) in &desc_match.limits_to_check {
                            let result = self
                                .execute_check_limit(key, limit, desc_match.operation)
                                .await;
                            match result {
                                Ok(decision) => {
                                    if !decision.allowed {
                                        any_rule_over_limit = true;
                                    }
                                    successful_decisions.push((limit, decision));
                                }
                                Err(error) => {
                                    if first_redis_error.is_none() {
                                        first_redis_error = Some(error.clone());
                                    }
                                    failed_rules.push((limit, error));
                                }
                            }
                        }

                        let has_over_limit =
                            successful_decisions.iter().any(|(_, dec)| !dec.allowed);

                        if has_over_limit {
                            let violated: Vec<(&RateLimit, &Decision)> = successful_decisions
                                .iter()
                                .filter(|(_, dec)| !dec.allowed)
                                .map(|(l, d)| (*l, d))
                                .collect();
                            let status = aggregate_descriptor_status(&violated, default_ttl);
                            statuses.push(status);
                        } else if failed_rules.is_empty() {
                            let pairs: Vec<(&RateLimit, &Decision)> =
                                successful_decisions.iter().map(|(l, d)| (*l, d)).collect();
                            let status = aggregate_descriptor_status(&pairs, default_ttl);
                            statuses.push(status);
                        } else {
                            let gov_limit = successful_decisions
                                .first()
                                .map(|(l, _)| **l)
                                .unwrap_or(desc_match.limits_to_check[0].1);
                            let reset_secs = duration_until_reset_for(&gov_limit, default_ttl);
                            statuses.push(DescriptorStatus {
                                code: Code::Unknown as i32,
                                current_limit: Some(gov_limit.to_proto()),
                                limit_remaining: 0,
                                duration_until_reset: Some(prost_types::Duration {
                                    seconds: reset_secs as i64,
                                    nanos: 0,
                                }),
                                quota: None,
                            });
                        }
                    }
                }
            }

            (statuses, any_rule_over_limit, first_redis_error)
        })
        .await;

        let (statuses, any_rule_over_limit, first_redis_error) = match eval_result {
            Ok(result) => result,
            Err(_elapsed) => {
                count(&self.metrics, "redis.timeouts", 1);
                count(&self.metrics, "requests.deadline_exceeded", 1);
                return Err(tonic::Status::deadline_exceeded(
                    "request execution deadline exceeded",
                ));
            }
        };

        // 5. Overall response code aggregation & F06 Error Precedence Rule
        if any_rule_over_limit {
            // Rule 1: Definitive quota rejection WINS!
            count(&self.metrics, "requests.over_limit", 1);
            warn!(domain = %request.domain, "request is over the rate limit");
            info!(
                domain = %request.domain,
                over_limit = true,
                "rate limit decision complete"
            );
            Ok(Response::new(build_response(true, statuses)))
        } else if let Some(err) = first_redis_error {
            // Rule 2: If NO rule was over limit, but one or more backend operations failed with a Redis error
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
            info!(
                domain = %request.domain,
                over_limit = false,
                "rate limit decision complete"
            );
            Ok(Response::new(build_response(false, statuses)))
        }
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
            rate_limit_key("domain", "default", &first_path, &limit, 10),
            rate_limit_key("domain", "default", &second_path, &limit, 10)
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

        let status =
            aggregate_descriptor_status(&[(&limit_sec, &dec_sec), (&limit_min, &dec_min)], 10);

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

        let status =
            aggregate_descriptor_status(&[(&limit_sec, &dec_sec), (&limit_min, &dec_min)], 10);

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
        let reset = duration_until_reset_for(&limit, 10);
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
    async fn caller_authorization_and_unsupported_refunds() {
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

        // 1. Untrusted caller attempting negative hits -> PermissionDenied
        let req_untrusted = RateLimitRequest {
            domain: "default".to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "type".to_string(),
                    value: "fixed".to_string(),
                }],
                limit: None,
                hits_addend: Some(2),
                is_negative_hits: true,
            }],
            hits_addend: 0,
        };
        let res = steward
            .should_rate_limit(tonic::Request::new(req_untrusted))
            .await;
        assert_eq!(res.unwrap_err().code(), Code::PermissionDenied);

        // 2. Trusted caller attempting negative hits on Sliding Window -> FailedPrecondition
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
        let mut grpc_req = tonic::Request::new(req_sliding);
        grpc_req
            .metadata_mut()
            .insert("x-steward-trusted", "true".parse().unwrap());
        let res = steward.should_rate_limit(grpc_req).await;
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

    fn spawn_dedicated_ephemeral_redis() -> Option<(u16, std::process::Child)> {
        static DEDICATED_PORT: std::sync::atomic::AtomicU16 =
            std::sync::atomic::AtomicU16::new(17500);
        let port = DEDICATED_PORT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: std::time::Duration::from_secs(1),
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
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
            10,
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
            10,
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
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
        let rkey = rate_limit_key(
            domain,
            matched.policy_id,
            &path,
            &matched.rate_limits[0],
            10,
        );
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
            let rkey = rate_limit_key(
                domain,
                matched.policy_id,
                &path,
                &matched.rate_limits[0],
                10,
            );
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
        let rkey = rate_limit_key(
            domain,
            matched.policy_id,
            &path,
            &matched.rate_limits[0],
            10,
        );
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
        let rkey = rate_limit_key(
            domain,
            matched.policy_id,
            &path,
            &matched.rate_limits[0],
            10,
        );
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: super::DEFAULT_EXECUTION_TIMEOUT,
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                super::DEFAULT_MAX_CONCURRENT_REQUESTS,
            )),
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: std::time::Duration::from_millis(10),
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1024)),
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
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
            )),
            scripts: super::StewardScripts::default(),
            execution_timeout: std::time::Duration::from_millis(10),
            admission_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1024)),
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
            "elapsed was {:?}",
            elapsed
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
            "elapsed was {:?}",
            elapsed
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
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit, 10);

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
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit, 10);

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
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit, 10);

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
        let key = rate_limit_key(domain, matched.policy_id, &path, &limit, 10);

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
}
