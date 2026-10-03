# Correctness and Protocol Qualification Matrix Results

- **Milestone:** M4.1 — Release Qualification
- **Date:** October 2026
- **Status:** Ratified & Verified (100% Pass)
- **Target Artifact:** `ghcr.io/cetanu/steward:latest` (Release container built on `ubuntu:noble` with pinned protobuf closure)
- **Reference Document:** [Production Readiness Review](../production-readiness.md)

---

## 1. Executive Summary

Milestone M4.1 executes the complete correctness and protocol qualification matrix ratified in [docs/production-readiness.md](../production-readiness.md#qualification-matrix) against the hardened production release artifact.

All 93 automated unit/component tests and the end-to-end containerized Envoy v1.39.0 integration test suite executed with a **100% pass rate** (0 failures, 0 regressions, 0 skipped tests). Every requirement across Matching, Protocol, Algorithms, Concurrency, Failures, Configuration, and Telemetry has been certified against the release artifact.

---

## 2. Qualification Environment

The matrix was executed in the official qualification environment using pinned dependencies and production-equivalent topologies:

| Component | Version / Specification | Deployment Context |
| --- | --- | --- |
| **Steward Service** | `v0.1.0` (Git commit `ce048ad`) | Multi-replica release artifact (`server` & `server-replica-2`), non-root UID 10001, AWS-LC crypto provider |
| **Envoy Proxy** | `v1.39.0` | Official Envoy image with HTTP connection manager, `enable_x_ratelimit_headers: DRAFT_VERSION_03`, failure-mode-deny: false |
| **Redis Backend** | `7.2.4` / Engine `7.2` | Dedicated primary with async connection manager, Lua script caching, memory eviction `noeviction` |
| **Config Provider** | HTTP REST & File | REST configuration server (`mock_config`) with HTTP conditional headers (ETag / If-None-Match) & local YAML/JSON fallback |
| **Base OS / Libc** | Ubuntu 24.04 LTS (`noble`) | GLIBC 2.38 runtime compatibility |

---

## 3. Qualification Matrix Traceability & Execution Results

### 3.1 Matching Scenarios

| Scenario | Required Behavior | Automated Test Coverage | Status |
| --- | --- | --- | --- |
| **Domain Isolation** | Domain routing enforces catalog isolation; unconfigured domains return unconstrained decisions (`OK`, `limit = None`). | `service::tests::unconfigured_domain_returns_unconstrained_status_for_all_descriptors` | **PASS** |
| **Ordered Hierarchy** | Path entries `[(k1, v1), (k2, v2)]` are strictly evaluated in sequence; order permutation produces distinct counter keys. | `rate_limits::tests::ordering_sensitivity`, `service::tests::fixed_window_hierarchy_ordering_and_tenant_isolation` | **PASS** |
| **Exact vs. Wildcard** | Deterministic evaluation prefers exact value over wildcard match; wildcard captures runtime request value into counter identity. | `rate_limits::tests::exact_vs_wildcard_precedence`, `rate_limits::tests::dynamic_counter_key_for_wildcards`, `service::tests::fixed_window_wildcard_dynamic_counter_creation` | **PASS** |
| **Parent Scope Isolation** | Child descriptor matches are isolated to their parent scope without cross-branch leakage. | `rate_limits::tests::parent_scope_isolation` | **PASS** |
| **Multi-Window Rules** | Multiple limits on a single descriptor path (e.g. second and minute) are evaluated and aggregated deterministically. | `rate_limits::tests::multiple_limits_on_one_descriptor_path`, `service::tests::multi_window_status_aggregation_allowed`, `service::tests::multi_window_status_aggregation_over_limit` | **PASS** |
| **Duplicate Descriptors** | Repeated descriptors within a single request are charged additively and preserve 1:1 input order in response statuses. | `service::tests::fixed_window_duplicate_descriptors_charged_additively_and_preserve_order` | **PASS** |
| **Unmatched Values** | Descriptors not matching any configured policy rule return unconstrained status (`Code::Ok`, `limit = None`). | `service::tests::unmatched_descriptor_returns_unconstrained_status`, `service::tests::unmatched_descriptors_populate_unconstrained_status_code_ok_limit_none` | **PASS** |

### 3.2 Protocol & Dimension Bounds

| Scenario | Required Behavior | Automated Test Coverage | Status |
| --- | --- | --- | --- |
| **Weighted Hits** | Descriptor-level `hits_addend` takes precedence over request-level `hits_addend`; charges exact token/event cost. | `service::tests::descriptor_hits_addend_takes_precedence_over_request_level`, `service::tests::fixed_window_weighted_hits_increment_counter_by_exact_cost` | **PASS** |
| **Zero-Cost Probes** | `hits_addend == 0` evaluates remaining quota and returns definitive decision without incrementing counter state. | `service::tests::zero_cost_probe_returns_remaining_without_mutating_redis` | **PASS** |
| **Refunds (`is_negative_hits`)** | Authorized callers can decrement counter state clamped at 0; sliding window rejects refunds with `FailedPrecondition`; untrusted callers rejected with `PermissionDenied`. | `service::tests::fixed_window_refund_decrements_and_clamps_at_zero`, `service::tests::caller_authorization_and_unsupported_refunds`, `service::tests::test_token_bucket_certification_direct_redis_state_and_refunds` | **PASS** |
| **Overrides** | Valid `RateLimitOverride` applies custom capacity; malformed overrides (`requests_per_unit == 0` or invalid unit) return `InvalidArgument`. | `rate_limits::tests::validate_override_rejects_malformed_inputs`, `service::tests::fixed_window_override_application_and_validation` | **PASS** |
| **Dimension Bounds** | Payloads exceeding bounds (domain > 128 B, > 16 descriptors, > 8 entries/descriptor, entry key/val > 256 B, hit cost > 100) are rejected with `InvalidArgument`. | `service::tests::request_validation_rejects_overbound_dimensions_and_malformed_inputs`, `service::tests::requests_exceeding_max_sliding_hit_cost_are_rejected` | **PASS** |
| **Stable Counter Identity** | Counter keys exclude mutable capacity thresholds, ensuring threshold changes reuse state without resetting consumed quota. | `rate_limits::tests::changing_capacity_threshold_does_not_alter_counter_key_identity`, `rate_limits::tests::canonical_key_format_matches_accounting_contract` | **PASS** |
| **Status Shape & Order** | Response `statuses` strictly mirrors input `descriptors` in 1:1 order, populating `current_limit`, `limit_remaining`, and `duration_until_reset`. | `service::tests::direct_grpc_assertions_ordered_statuses_limits_remaining_and_reset` | **PASS** |

### 3.3 Algorithm Certification

| Scenario | Required Behavior | Automated Test Coverage | Status |
| --- | --- | --- | --- |
| **Fixed Window** | Exact quota boundaries (`1..capacity` allowed, `capacity+1` denied), window boundary reset after duration, INCRBY/EXPIRE atomicity. | `service::tests::fixed_window_exact_quota_boundary_assertions`, `service::tests::fixed_window_expiry_resets_quota_after_duration`, `service::tests::test_duration_until_reset_bounds` | **PASS** |
| **Token Bucket Capacity & Burst** | Full capacity exhaustion, fractional refill proportional to elapsed milliseconds (`refill_per_ms`), bursts clamped at capacity. | `service::tests::test_token_bucket_certification_capacity_exhaustion`, `service::tests::test_token_bucket_certification_fractional_refill_over_time`, `service::tests::test_reference_models_pure_logic` | **PASS** |
| **Token Bucket Clock Regression** | Authoritative Redis time (`TIME`), elapsed time calculation clamped at 0 on backwards clock drift without spurious token refills. | `service::tests::test_token_bucket_certification_clock_drift_and_backward_shift_protection`, `service::tests::token_bucket_withstands_simulated_clock_regressions_without_extra_refills` | **PASS** |
| **Token Bucket Dynamic Update** | Live threshold capacity update preserves consumed token proportion without resetting state. | `service::tests::test_token_bucket_certification_dynamic_capacity_update_live_keys` | **PASS** |
| **Sliding Window Log & Boundaries** | Exact log boundary expiry via `ZREMRANGEBYSCORE`, event retention cap at 10,000 entries via `ZREMRANGEBYRANK`. | `service::tests::test_sliding_window_certification_exact_log_boundary_expiry`, `service::tests::sliding_window_prunes_expired_entries_and_respects_10000_event_cap`, `service::tests::test_sliding_window_certification_10000_event_retention_cap` | **PASS** |
| **Sliding Window Collision Free** | Unique member format `<usec>:<128-bit-nonce>:<i>` ensures concurrent multi-replica calls at the same millisecond never overwrite events. | `service::tests::sliding_window_concurrent_replicas_same_millisecond_do_not_overwrite_events`, `service::tests::test_sliding_window_certification_multi_replica_concurrency`, `service::tests::test_sliding_window_certification_cardinality_and_membership_format` | **PASS** |

### 3.4 Concurrency & Overload Protection

| Scenario | Required Behavior | Automated Test Coverage | Status |
| --- | --- | --- | --- |
| **Multi-Rule Concurrency** | Evaluates multiple matched rules concurrently using `futures::future::join_all`, preserving 1:1 descriptor ordering in statuses. | `service::tests::test_concurrent_multi_rule_evaluation_and_ordering` | **PASS** |
| **Global Admission Control** | Non-blocking semaphore rejects excess concurrent requests beyond `max_concurrent_requests` (1,024) with `ResourceExhausted`. | `service::tests::test_global_admission_limit_sheds_excess_load`, `service::tests::test_global_admission_limit_permits_retained_during_execution_and_released` | **PASS** |
| **Deadlines & Timeouts** | Parses gRPC `grpc-timeout` header, enforces effective execution timeout (10 ms), aborts slow checks with `DeadlineExceeded`. | `service::tests::test_parse_grpc_timeout`, `service::tests::test_effective_timeout_calculation`, `service::tests::test_request_deadline_expiration_returns_deadline_exceeded` | **PASS** |
| **In-Flight Tracking** | RAII `InFlightGuard` maintains atomic gauge `in_flight_requests` accurately across successful, failed, and shed calls. | `service::tests::test_in_flight_gauge_lifecycle` | **PASS** |
| **HTTP/2 Connection Balancing** | Balances concurrent streams across multiple RLS server replicas with round-robin distribution. | `service::tests::test_multi_replica_http2_load_balancing_and_timeout` | **PASS** |

### 3.5 Failure Modes & Fault Tolerance

| Scenario | Required Behavior | Automated Test Coverage | Status |
| --- | --- | --- | --- |
| **F06 Error Precedence** | Definitive quota rejection (`OVER_LIMIT`) takes precedence; unresolved Redis failures return `Unavailable` or `DeadlineExceeded` (never converted to `OK`). | `service::tests::definitive_denial_precedence_when_rule1_over_limit_and_rule2_redis_error`, `service::tests::redis_error_returns_unavailable_when_no_rule_over_limit` | **PASS** |
| **Redis Mid-Flight Failure** | Sudden Redis disconnection mid-request returns `tonic::Status::unavailable` and increments `redis.errors`. | `service::tests::test_redis_killed_mid_flight_returns_unavailable` | **PASS** |
| **Redis Failover & Recovery** | `redis::aio::ConnectionManager` reconnects after simulated Redis restart/failover, cleanly resuming traffic without service restart. | `service::tests::test_redis_failover_and_reconnection_resumes_cleanly` | **PASS** |
| **`NOSCRIPT` Cache Recovery** | Intercepts `NOSCRIPT` error after Redis `SCRIPT FLUSH`, automatically reloads Lua scripts via `script.load_async`, and retries transparently. | `service::tests::test_noscript_cache_recovery_after_script_flush` | **PASS** |
| **Redis URL Validation** | Decouples Redis connection string into validated URL supporting credentials, database index, and TLS (`rediss://`). | `service::tests::test_redis_url_normalization_and_sanitization`, `config_source::tests::settings_redis_target_precedence` | **PASS** |

### 3.6 Configuration Lifecycle & Gated Startup

| Scenario | Required Behavior | Automated Test Coverage | Status |
| --- | --- | --- | --- |
| **Gated Startup** | Server blocks gRPC listener until initial config snapshot is loaded, validated, and compiled. | `config_source::tests::startup_fails_and_stays_unready_on_invalid_config`, `config_source::tests::load_initial_rate_limits_zero_budget_fails_immediately` | **PASS** |
| **Startup Retry Budget** | Retries initial HTTP configuration fetch with exponential backoff up to `startup_timeout_secs` (30s) before aborting. | `config_source::tests::load_initial_rate_limits_retries_and_succeeds_when_source_appears`, `config_source::tests::load_initial_rate_limits_times_out_and_reports_attempts`, `config_source::tests::load_initial_rate_limits_succeeds_immediately_for_valid_source` | **PASS** |
| **Whole-Snapshot Validation** | Rejects non-positive capacities, oversized limits (> u32::MAX), unknown units, and duplicate paths during compilation. | `config_source::tests::validation_rejects_empty_configuration`, `config_source::tests::validation_rejects_non_positive_capacity_at_compile_time`, `config_source::tests::validation_rejects_oversized_capacity_at_compile_time`, `config_source::tests::validation_rejects_unknown_units_at_compile_time`, `config_source::tests::validation_rejects_duplicate_path_and_unit_rules` | **PASS** |
| **Resilient Reloads** | Reload failure preserves active compiled snapshot without traffic interruption; exposes `config.age_seconds` and version hash. | `config_source::tests::reload_failure_preserves_active_configuration_snapshot`, `config_source::tests::compilation_generates_stable_version_hash_and_timestamp`, `service::tests::steward_exposes_active_version_hash_and_age` | **PASS** |
| **Streaming Payload Limits** | Rejects HTTP config payloads exceeding `MAX_CONFIG_PAYLOAD_BYTES` (16 MiB) on both Content-Length and chunked streams. | `config_source::tests::http_oversized_content_length_rejected`, `config_source::tests::http_oversized_streaming_payload_rejected`, `config_source::tests::http_conditional_fetch_and_304_not_modified` | **PASS** |
| **Graceful Drain** | Updates gRPC Health status to `NOT_SERVING`, waits for configurable drain window before closing socket. | `service::tests::test_grpc_health_service_lifecycle_and_shutdown_drain` | **PASS** |

### 3.7 Telemetry & Integration

| Scenario | Required Behavior | Automated Test Coverage | Status |
| --- | --- | --- | --- |
| **StatsD Telemetry** | Emits metrics for allowed (`rpc.duration.allowed`), denied (`rpc.duration.denied`), timeouts (`redis.timeouts`), and admission drops. | `service::tests::test_telemetry_metrics_allowed_and_denied_flow`, `service::tests::test_telemetry_metrics_on_admission_load_shedding`, `metrics::tests::statsd_queue_drop_tracking`, `metrics::tests::error_rate_limiter_throttles_rapid_invocations` | **PASS** |
| **Live Envoy Integration** | Live containerized Envoy v1.39.0 evaluates HTTP traffic, validates RFC draft-03 headers (`x-ratelimit-limit`, `x-ratelimit-remaining`, `x-ratelimit-reset`) on 200 OK and 429 Too Many Requests. | `tests/rate_limit.rs::envoy_allows_requests_then_returns_rate_limit_response` | **PASS** |

---

## 4. Release Decision & Conclusion

1. **Qualification Matrix Compliance:** 100% (all required scenarios implemented and certified).
2. **Automated Verification:** 93 unit/component tests + live Docker integration suite passing in CI with 0 warnings or errors.
3. **Artifact Integrity:** Production container image qualified with non-root security context, AWS-LC cryptography (zero `ring` dependencies), and zero vendored `.proto` files in Git.
4. **Sign-off:** Milestone M4.1 criteria are fully satisfied. The service is qualified to proceed to M4.2 (offered-load sweeps and soak testing).
