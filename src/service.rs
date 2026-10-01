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
use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;
use crate::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};
use crate::rate_limits::{Algorithm, Descriptor, RateLimit};
use crate::response::limit_response;

pub type RateLimitConfigs = HashMap<String, Vec<Descriptor>>;

const FIXED_WINDOW_SCRIPT: &str = include_str!("scripts/fixed_window.lua");
const TOKEN_BUCKET_SCRIPT: &str = include_str!("scripts/token_bucket.lua");
const SLIDING_WINDOW_SCRIPT: &str = include_str!("scripts/sliding_window.lua");

static REQUEST_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
struct Decision {
    allowed: bool,
    observed: i64,
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

fn matching_rate_limits(
    request: &RateLimitRequest,
    configured_limits: &[Descriptor],
) -> HashMap<String, RateLimit> {
    let mut matches = HashMap::new();
    for request_descriptor in &request.descriptors {
        let override_ = request_descriptor.limit.as_ref();
        for entry in &request_descriptor.entries {
            for configured in configured_limits {
                if configured.key != entry.key || configured.value != entry.value {
                    continue;
                }

                let limit = override_
                    .map(|override_| configured.rate_limit.with_override(override_))
                    .unwrap_or_else(|| configured.rate_limit.clone());
                if !limit.is_valid() {
                    warn!(descriptor_key = %configured.key, descriptor_value = %configured.value, "ignoring invalid rate limit");
                    continue;
                }

                matches.insert(rate_limit_key(&request.domain, configured, &limit), limit);
            }
        }
    }
    matches
}

fn rate_limit_key(domain: &str, descriptor: &Descriptor, limit: &RateLimit) -> String {
    format!(
        "steward:rate:{}:{domain}:{}:{}:{}:{}:{}:{}:{}",
        domain.len(),
        descriptor.key.len(),
        descriptor.key,
        descriptor.value.len(),
        descriptor.value,
        limit.requests_per_unit,
        limit.unit.as_str(),
        // Keep state separate when an algorithm is changed in configuration.
        limit.algorithm.as_str()
    )
}

#[tonic::async_trait]
impl RateLimitService for Steward {
    async fn should_rate_limit(
        &self,
        request: tonic::Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, tonic::Status> {
        count(&self.metrics, "requests.total", 1);
        let request = request.into_inner();
        let Some(configured_limits) = self.config_rx.borrow().get(&request.domain).cloned() else {
            count(&self.metrics, "requests.unconfigured", 1);
            return Ok(Response::new(limit_response(false)));
        };

        let limits = matching_rate_limits(&request, &configured_limits);
        let hits = i64::from(request.hits_addend.max(1));
        debug!(domain = %request.domain, limits = limits.len(), "checking rate limits");

        let decisions: HashMap<String, Decision> = tokio::task::block_in_place(|| {
            limits
                .iter()
                .map(|(key, limit)| {
                    let decision = self.check_limit_fail_open(key, limit, hits);
                    gauge(
                        &self.metrics,
                        "rate_limit.observed",
                        decision.observed.max(0) as u64,
                    );
                    (key.clone(), decision)
                })
                .collect()
        });

        let over_limit = decisions.values().any(|decision| !decision.allowed);
        if over_limit {
            count(&self.metrics, "requests.over_limit", 1);
            warn!(domain = %request.domain, "request is over the rate limit");
        } else {
            count(&self.metrics, "requests.allowed", 1);
        }

        info!(domain = %request.domain, over_limit, "rate limit decision complete");
        Ok(Response::new(limit_response(over_limit)))
    }
}

#[cfg(test)]
mod tests {
    use super::rate_limit_key;
    use crate::rate_limits::{Algorithm, Descriptor, RateLimit, Unit};

    #[test]
    fn descriptor_keys_do_not_collide_when_values_share_prefixes() {
        let first = Descriptor {
            key: "a".to_owned(),
            value: "bc".to_owned(),
            rate_limit: RateLimit {
                algorithm: Algorithm::FixedWindow,
                unit: Unit::Seconds,
                requests_per_unit: 1,
            },
        };
        let second = Descriptor {
            key: "ab".to_owned(),
            value: "c".to_owned(),
            ..first.clone()
        };

        assert_ne!(
            rate_limit_key("domain", &first, &first.rate_limit),
            rate_limit_key("domain", &second, &second.rate_limit)
        );
    }
}
