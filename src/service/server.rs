use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use cadence::{NopMetricSink, StatsdClient};
use redis::Script;
use redis::aio::ConnectionManager;
use tokio::sync::watch::Receiver;
use tonic::Response;
use tracing::{debug, error};

use crate::config_source::CompiledConfig;
use crate::metrics::{ErrorRateLimiter, SharedMetrics, count, gauge, time};
use crate::proto::envoy::service::ratelimit::v3::rate_limit_response::{Code, DescriptorStatus};
use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;
use crate::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};
use crate::rate_limits::{Algorithm, RateLimit, encode_canonical_path, rate_limit_key};
use crate::response::build_response;

use super::scripts::{
    Decision, HitOperation, ScriptOutcome, StewardScripts, generate_sliding_window_nonce,
};
use super::status::{aggregate_descriptor_status, duration_until_reset_for};
use super::validation::{
    compute_descriptor_hit_cost, is_redis_timeout, normalize_redis_url, parse_grpc_timeout,
    sanitize_url, validate_request,
};
use super::{DEFAULT_EXECUTION_TIMEOUT, DEFAULT_MAX_CONCURRENT_REQUESTS, RateLimitConfigs};

pub(crate) struct DescriptorMatch {
    pub(crate) limits_to_check: Vec<(String, RateLimit)>,
    pub(crate) operation: HitOperation,
}

pub(crate) enum DescriptorEvaluation {
    Unmatched,
    Matched(DescriptorMatch),
}

pub(crate) enum PreparedRequest {
    Unconfigured(Vec<DescriptorStatus>),
    Configured(Vec<DescriptorEvaluation>),
}

#[derive(Clone)]
pub struct Steward {
    pub(crate) config_rx: Receiver<RateLimitConfigs>,
    pub(crate) redis: ConnectionManager,
    pub(crate) metrics: SharedMetrics,
    pub(crate) scripts: StewardScripts,
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

    fn prepare_request(
        &self,
        request: &RateLimitRequest,
        rpc_start: std::time::Instant,
    ) -> Result<PreparedRequest, tonic::Status> {
        validate_request(request).inspect_err(|_| {
            count(&self.metrics, "requests.invalid", 1);
            time(&self.metrics, "rpc.duration", rpc_start.elapsed());
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
