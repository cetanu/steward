use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
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
use crate::rate_limits::{Algorithm, PolicyTrie, RateLimit, encode_canonical_path, rate_limit_key};
use crate::response::{build_response, limit_response};

pub type RateLimitConfigs = HashMap<String, PolicyTrie>;

const FIXED_WINDOW_SCRIPT: &str = include_str!("scripts/fixed_window.lua");
const TOKEN_BUCKET_SCRIPT: &str = include_str!("scripts/token_bucket.lua");
const SLIDING_WINDOW_SCRIPT: &str = include_str!("scripts/sliding_window.lua");

static REQUEST_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct Decision {
    pub allowed: bool,
    pub observed: i64,
}

struct DescriptorMatch {
    limits_to_check: Vec<(String, RateLimit)>,
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

    fn check_limit(&self, key: &str, limit: &RateLimit, hits: i64) -> redis::RedisResult<Decision> {
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

    fn check_limit_fail_open(&self, key: &str, limit: &RateLimit, hits: i64) -> Decision {
        let started = std::time::Instant::now();
        let decision = match self.check_limit(key, limit, hits) {
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

#[tonic::async_trait]
impl RateLimitService for Steward {
    async fn should_rate_limit(
        &self,
        request: tonic::Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, tonic::Status> {
        count(&self.metrics, "requests.total", 1);
        let request = request.into_inner();

        // 1. In-memory Hierarchical Match Phase (precompiled trie lookup)
        let evaluations: Vec<DescriptorEvaluation> = {
            let configs = self.config_rx.borrow();
            let Some(domain_policy) = configs.get(&request.domain) else {
                count(&self.metrics, "requests.unconfigured", 1);
                return Ok(Response::new(limit_response(false)));
            };

            let mut evals = Vec::with_capacity(request.descriptors.len());
            for req_desc in &request.descriptors {
                if req_desc.entries.is_empty() {
                    evals.push(DescriptorEvaluation::Unmatched);
                    continue;
                }

                let entries: Vec<(&str, &str)> = req_desc
                    .entries
                    .iter()
                    .map(|e| (e.key.as_str(), e.value.as_str()))
                    .collect();

                let match_result = domain_policy.match_entries(&entries);
                match match_result {
                    Some(res) => {
                        let encoded_path = encode_canonical_path(entries);
                        let override_ = req_desc.limit.as_ref();
                        let mut limits_to_check = Vec::new();

                        for configured_limit in res.rate_limits {
                            let effective_limit = override_
                                .map(|o| configured_limit.with_override(o))
                                .unwrap_or_else(|| *configured_limit);

                            if !effective_limit.is_valid() {
                                warn!(
                                    domain = %request.domain,
                                    "ignoring invalid rate limit"
                                );
                                continue;
                            }

                            let key = rate_limit_key(
                                &request.domain,
                                res.policy_id,
                                &encoded_path,
                                &effective_limit,
                                self.default_ttl,
                            );
                            limits_to_check.push((key, effective_limit));
                        }

                        if limits_to_check.is_empty() {
                            evals.push(DescriptorEvaluation::Unmatched);
                        } else {
                            evals.push(DescriptorEvaluation::Matched(DescriptorMatch {
                                limits_to_check,
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

        let hits = i64::from(request.hits_addend.max(1));
        debug!(
            domain = %request.domain,
            descriptors = request.descriptors.len(),
            "evaluating rate limits"
        );

        // 2. Redis Evaluation Phase (preserving 1:1 input descriptor order)
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
                            let decision = self.check_limit_fail_open(key, limit, hits);
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

        // 3. Overall response code aggregation
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
}
