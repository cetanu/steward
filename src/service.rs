use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use cadence::{NopMetricSink, StatsdClient};
use redis::Script;
use tokio::sync::watch::Receiver;
use tonic::Response;
use tracing::{debug, error, info, warn};

use crate::metrics::{SharedMetrics, count, gauge, time};
use crate::proto::envoy::service::ratelimit::v3::rate_limit_response::{Code, DescriptorStatus};
use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;
use crate::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};
use crate::rate_limits::{Algorithm, RateLimit, encode_canonical_path, rate_limit_key};
use crate::response::{build_response, limit_response};

pub use crate::config_source::CompiledConfig;

pub type RateLimitConfigs = Arc<CompiledConfig>;

const FIXED_WINDOW_SCRIPT: &str = include_str!("scripts/fixed_window.lua");
const FIXED_WINDOW_REFUND_SCRIPT: &str = include_str!("scripts/fixed_window_refund.lua");
const TOKEN_BUCKET_SCRIPT: &str = include_str!("scripts/token_bucket.lua");
const TOKEN_BUCKET_REFUND_SCRIPT: &str = include_str!("scripts/token_bucket_refund.lua");
const SLIDING_WINDOW_SCRIPT: &str = include_str!("scripts/sliding_window.lua");

static REQUEST_NONCE: AtomicU64 = AtomicU64::new(0);

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

struct DescriptorMatch {
    limits_to_check: Vec<(String, RateLimit)>,
    operation: HitOperation,
}

enum DescriptorEvaluation {
    Unmatched,
    Matched(DescriptorMatch),
}

pub struct Steward {
    config_rx: Receiver<RateLimitConfigs>,
    redis_pool: r2d2::Pool<redis::Client>,
    default_ttl: usize,
    metrics: SharedMetrics,
}

impl Steward {
    /// Construct a service with metrics disabled.
    pub fn new(
        redis_host: &str,
        default_ttl: usize,
        config_rx: Receiver<RateLimitConfigs>,
        pool_size: usize,
    ) -> Self {
        Self::try_new(
            redis_host,
            default_ttl,
            config_rx,
            pool_size,
            std::sync::Arc::new(StatsdClient::from_sink("", NopMetricSink)),
        )
        .expect("failed to create Steward service")
    }

    /// Construct a service and return configuration or pool initialization errors.
    pub fn try_new(
        redis_host: &str,
        default_ttl: usize,
        config_rx: Receiver<RateLimitConfigs>,
        pool_size: usize,
        metrics: SharedMetrics,
    ) -> Result<Self, String> {
        let manager = redis::Client::open(format!("redis://{redis_host}"))
            .map_err(|error| format!("invalid Redis configuration: {error}"))?;
        let redis_pool = r2d2::Pool::builder()
            .max_size(pool_size.max(1) as u32)
            .build(manager)
            .map_err(|error| format!("failed to create Redis connection pool: {error}"))?;

        Ok(Self {
            config_rx,
            redis_pool,
            default_ttl,
            metrics,
        })
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
    pub fn for_test(config_rx: Receiver<RateLimitConfigs>) -> Self {
        let manager = redis::Client::open("redis://127.0.0.1:6379").unwrap();
        let redis_pool = r2d2::Pool::builder()
            .min_idle(Some(0))
            .build_unchecked(manager);
        Self {
            config_rx,
            redis_pool,
            default_ttl: 10,
            metrics: Arc::new(StatsdClient::from_sink("", NopMetricSink)),
        }
    }

    pub fn check_limit(
        &self,
        key: &str,
        limit: &RateLimit,
        op: HitOperation,
    ) -> redis::RedisResult<Decision> {
        let mut connection = self.redis_pool.get().map_err(|error| {
            redis::RedisError::from((
                redis::ErrorKind::Io,
                "failed to acquire Redis connection",
                error.to_string(),
            ))
        })?;
        let window_seconds = limit
            .unit
            .seconds()
            .unwrap_or(self.default_ttl as u64)
            .max(1);

        match op {
            HitOperation::Probe => match limit.algorithm {
                Algorithm::FixedWindow => {
                    let current: Option<i64> =
                        redis::cmd("GET").arg(key).query(&mut *connection)?;
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
                        .query(&mut *connection)?;
                    let tokens = match state.0 {
                        None => capacity,
                        Some(t) => {
                            let elapsed = (now_millis() - state.1.unwrap_or(0)).max(0);
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
                    let count: i64 = redis::cmd("ZCOUNT")
                        .arg(key)
                        .arg(now_millis() - window_ms)
                        .arg("+inf")
                        .query(&mut *connection)?;
                    Ok(Decision {
                        allowed: count <= limit.requests_per_unit,
                        observed: count,
                    })
                }
            },
            HitOperation::Refund(hits) => match limit.algorithm {
                Algorithm::FixedWindow => {
                    let current: i64 = Script::new(FIXED_WINDOW_REFUND_SCRIPT)
                        .key(key)
                        .arg(hits as i64)
                        .invoke(&mut *connection)?;
                    Ok(Decision {
                        allowed: true,
                        observed: current,
                    })
                }
                Algorithm::TokenBucket => {
                    let window_ms = window_seconds.saturating_mul(1_000).max(1);
                    let capacity = limit.requests_per_unit as f64;
                    let refill_per_ms = capacity / window_ms as f64;
                    let result: Vec<i64> = Script::new(TOKEN_BUCKET_REFUND_SCRIPT)
                        .key(key)
                        .arg(now_millis())
                        .arg(capacity)
                        .arg(refill_per_ms)
                        .arg(hits as i64)
                        .arg(window_ms)
                        .invoke(&mut *connection)?;
                    Ok(Decision {
                        allowed: result.first().copied().unwrap_or(1) == 1,
                        observed: result.get(1).copied().unwrap_or_default(),
                    })
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
                        let current: i64 = Script::new(FIXED_WINDOW_SCRIPT)
                            .key(key)
                            .arg(hits)
                            .arg(window_seconds)
                            .invoke(&mut *connection)?;
                        Ok(Decision {
                            allowed: current <= limit.requests_per_unit,
                            observed: current,
                        })
                    }
                    Algorithm::TokenBucket => {
                        let window_ms = window_seconds.saturating_mul(1_000).max(1);
                        let capacity = limit.requests_per_unit as f64;
                        let refill_per_ms = capacity / window_ms as f64;
                        let result: Vec<i64> = Script::new(TOKEN_BUCKET_SCRIPT)
                            .key(key)
                            .arg(now_millis())
                            .arg(capacity)
                            .arg(refill_per_ms)
                            .arg(hits)
                            .arg(window_ms)
                            .invoke(&mut *connection)?;
                        Ok(Decision {
                            allowed: result.first().copied().unwrap_or_default() == 1,
                            observed: result.get(1).copied().unwrap_or_default(),
                        })
                    }
                    Algorithm::SlidingWindow => {
                        let window_ms = window_seconds.saturating_mul(1_000).max(1);
                        let nonce = REQUEST_NONCE.fetch_add(1, Ordering::Relaxed);
                        let nonce = format!("{}-{nonce}", now_millis());
                        let result: Vec<i64> = Script::new(SLIDING_WINDOW_SCRIPT)
                            .key(key)
                            .arg(now_millis())
                            .arg(window_ms)
                            .arg(limit.requests_per_unit)
                            .arg(hits)
                            .arg(nonce)
                            .invoke(&mut *connection)?;
                        Ok(Decision {
                            allowed: result.first().copied().unwrap_or_default() == 1,
                            observed: result.get(1).copied().unwrap_or_default(),
                        })
                    }
                }
            }
        }
    }

    fn check_limit_fail_open(&self, key: &str, limit: &RateLimit, op: HitOperation) -> Decision {
        let started = std::time::Instant::now();
        let decision = match self.check_limit(key, limit, op) {
            Ok(decision) => decision,
            Err(error) => {
                error!(rate_limit_key = key, %error, "failed to update rate limit in Redis");
                count(&self.metrics, "redis.errors", 1);
                Decision {
                    allowed: true,
                    observed: 0,
                }
            }
        };
        time(&self.metrics, "redis.operation_time", started.elapsed());
        decision
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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
                return Ok(Response::new(limit_response(false)));
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

        // 4. Redis Evaluation Phase (preserving 1:1 input descriptor order)
        let default_ttl = self.default_ttl;
        let statuses: Vec<DescriptorStatus> = tokio::task::block_in_place(|| {
            let mut statuses = Vec::with_capacity(evaluations.len());
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
                        let mut limit_decisions =
                            Vec::with_capacity(desc_match.limits_to_check.len());
                        for (key, limit) in &desc_match.limits_to_check {
                            let decision =
                                self.check_limit_fail_open(key, limit, desc_match.operation);
                            gauge(
                                &self.metrics,
                                "rate_limit.observed",
                                decision.observed.max(0) as u64,
                            );
                            limit_decisions.push((limit, decision));
                        }

                        let pairs: Vec<(&RateLimit, &Decision)> =
                            limit_decisions.iter().map(|(l, d)| (*l, d)).collect();
                        let status = aggregate_descriptor_status(&pairs, default_ttl);
                        statuses.push(status);
                    }
                }
            }
            statuses
        });

        // 5. Overall response code aggregation
        let overall_over = statuses.iter().any(|s| s.code == Code::OverLimit as i32);
        if overall_over {
            count(&self.metrics, "requests.over_limit", 1);
            warn!(domain = %request.domain, "request is over the rate limit");
        } else {
            count(&self.metrics, "requests.allowed", 1);
        }

        info!(
            domain = %request.domain,
            over_limit = overall_over,
            "rate limit decision complete"
        );
        Ok(Response::new(build_response(overall_over, statuses)))
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

    #[test]
    fn steward_exposes_active_version_hash_and_age() {
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

        let steward = super::Steward::for_test(rx);
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
        let steward = super::Steward::for_test(rx);

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

    #[test]
    fn zero_cost_probe_returns_remaining_without_mutating_redis() {
        let Some(_server) = TestRedisServer::start() else {
            eprintln!("Skipping zero-cost probe test: redis-server not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{}", _server.port)).unwrap();
        let pool = r2d2::Pool::builder().build(client.clone()).unwrap();
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
            redis_pool: pool,
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
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

    #[test]
    fn fixed_window_refund_decrements_and_clamps_at_zero() {
        let Some(_server) = TestRedisServer::start() else {
            eprintln!("Skipping fixed-window refund test: redis-server not available");
            return;
        };

        let client = redis::Client::open(format!("redis://127.0.0.1:{}", _server.port)).unwrap();
        let pool = r2d2::Pool::builder().build(client.clone()).unwrap();
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
            redis_pool: pool,
            default_ttl: 10,
            metrics: std::sync::Arc::new(cadence::StatsdClient::from_sink(
                "",
                cadence::NopMetricSink,
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
            .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.observed, 3);
        let val: i64 = redis::cmd("GET").arg(test_key).query(&mut conn).unwrap();
        assert_eq!(val, 3);

        // Refund 5 from counter at 3 -> clamped at 0 (never negative)
        let decision = steward
            .check_limit(test_key, &limit, super::HitOperation::Refund(5))
            .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.observed, 0);
        let val: i64 = redis::cmd("GET").arg(test_key).query(&mut conn).unwrap();
        assert_eq!(val, 0);

        // Clean up
        let _: () = redis::cmd("DEL").arg(test_key).query(&mut conn).unwrap();
    }
}
