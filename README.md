steward
============================================================

Mission Statement
------------------------------------------------------------

Steward is a high-performance, production-ready implementation of the Envoy Rate Limit Service (RLS v3) API in Rust, backed by Redis.


Features
------------------------------------------------------------

* Native Envoy Rate Limit Service (RLS v3) gRPC compatibility with optional TLS/mTLS support
* High-throughput, non-blocking asynchronous pipeline built on Tokio and Tonic
* Redis-backed atomic rate limiting with three certified algorithms: Fixed Window, Token Bucket, and Sliding Window
* Dynamic configuration loading and background hot-reloads from local files or HTTP endpoints
* Strict admission control, load shedding, and request deadline enforcement
* Resilient Redis connection management with automatic failover recovery and `NOSCRIPT` script cache healing
* Comprehensive StatsD metrics and standard gRPC health checking (`grpc.health.v1`)


Production Architecture & Qualification
------------------------------------------------------------

Steward's production operational requirements, accounting semantics, qualification evidence, and runbooks are documented in:

* [Supported Algorithms](docs/algorithms.md) — Mathematical models and Redis state machines for Fixed Window, Token Bucket, and Sliding Window.
* [Production Readiness Assessment](docs/production-readiness.md) — Milestone roadmap, core findings, and qualification matrix.
* [Redis Operational Guide](docs/redis-operational-guide.md) — Topology, sizing, failover, persistence, and memory policies.
* [Operational Runbooks](docs/runbooks/README.md) — Production incident triage, failover, and operational procedures.
* [Release Dossier v1.0](docs/release-qualification/release-dossier-v1.0.md) — Full GA qualification results, load profiles, chaos verification, and canary sign-off.
* [Load, Soak & Burst Qualification](docs/release-qualification/load-and-soak-results.md) — Soak stability, high-cardinality churn, and 3× burst verification.
* [Chaos & Resilience Results](docs/release-qualification/chaos-and-resilience-results.md) — Fault injection, partition, and failover verification.
* [Canary Verification Report](docs/release-qualification/canary-verification.md) — Stepwise canary rollout and divergence verification.



Building and testing
------------------------------------------------------------

### Prerequisites

* Docker
* Docker-compose
* Make

### Optional prerequisites for building locally

* Rust toolchain 1.88 or newer


### Building locally

Simply execute `cargo build --release` to create a binary
which, when run, will start the rate limit service as a
gRPC server.

### Local development environment

A complete local testbed can be brought up with `make run`.  
This starts an Envoy proxy, a Redis database, a mock configuration server,
and an httpbin backend for integration testing and verification within this repository.

> **Note:** In production, Steward is deployed as a standalone binary or container image
> directly into your own infrastructure (Kubernetes, ECS, Nomad, or bare-metal host).
> The mock server, Docker Compose setup, and testbed configs are strictly development harnesses.

### Running tests

The project includes unit tests, component benchmarks, and end-to-end integration tests:
* `cargo test` - Runs Rust unit and component tests.
* `make test` - Runs end-to-end integration tests against the local Docker Compose harness.


Configuration
------------------------------------------------------------

Steward resolves its configuration using the following precedence:
1. `STEWARD_CONFIG_PATH` environment variable (explicit file path or comma-separated list).
2. `./steward.yaml` or `./steward.yml` (current working directory).
3. `/etc/steward/steward.yaml` or `/etc/steward/steward.yml` (standard daemon path).
4. `/etc/steward.yaml` or `/etc/steward.yml` (standard system root path).
5. Environment variables (`STEWARD__*` and `REDIS_URL`), enabling fully file-less 12-factor deployments.

### Example configuration file

```yaml
listen:
  addr: 0.0.0.0
  port: 5001

rate_limit_configs:
  # Upstream HTTP endpoint:
  http: https://config-service.internal/v1/rate_limits
  # Or local file:
  # file: /etc/steward/rate_limits.json

# Redis connection target (supports redis:// and rediss:// for TLS):
redis_url: redis://127.0.0.1:6379

config_refresh_interval_secs: 60
max_stale_duration_secs: 3600
execution_timeout_ms: 10
max_concurrent_requests: 1024
```

### Environment variables (12-Factor)

All configuration options can be configured via environment variables, with no configuration file required:

* `STEWARD_CONFIG_PATH`: Path to configuration file(s).
* `REDIS_URL`: Redis counter store URL (`redis://...` or `rediss://...` for TLS).
* `STEWARD__LISTEN__ADDR`: Bind IPv4 address (e.g. `0.0.0.0`).
* `STEWARD__LISTEN__PORT`: Bind port (e.g. `5001`).
* `STEWARD__RATE_LIMIT_CONFIGS__HTTP`: URL for dynamic rate limit policies.
* `STEWARD__RATE_LIMIT_CONFIGS__FILE`: Path to local rate limit policy file.
* `STEWARD__CONFIG_REFRESH_INTERVAL_SECS`: Policy reload polling interval in seconds (default: 60).
* `STEWARD__MAX_STALE_DURATION_SECS`: Max cache duration before stale policy alert triggers (default: 3600).
* `STEWARD__EXECUTION_TIMEOUT_MS`: Internal Redis evaluation deadline (default: 10 ms).
* `STEWARD__MAX_CONCURRENT_REQUESTS`: Max in-flight admission limit before load shedding (default: 1024).

### `rate_limit_configs`

This parameter allows specifying a location for the service
to lookup various rate limit configurations.

Either a `Http` or `File` location can be specified.

Example of what the service expects the location to contain:

```json
{
    "domain": [
        {
            "key": "descriptor_key",
            "value": "descriptor_value",
            "rate_limit": {
                "unit": "<seconds|minutes|hours|days|months|years>",
                "requests_per_unit": 12345
            }
        }
    ]
}
```

There can be any number of domains and descriptors.

### Rate-limit algorithms

Each descriptor defaults to the `fixed_window` algorithm.
Set `algorithm` to `fixed_window`, `token_bucket`, or `sliding_window` for a
different policy:

```json
{
  "api": [
    {
      "key": "client",
      "value": "example",
      "rate_limit": {
        "algorithm": "token_bucket",
        "unit": "seconds",
        "requests_per_unit": 20
      }
    }
  ]
}
```

The token bucket uses `requests_per_unit` as both its burst capacity and its
refill rate over the selected unit. Sliding-window state is maintained as a
Redis sorted set. Redis updates are atomic, and a request that exactly reaches
the limit is allowed.

### StatsD metrics

Metrics are disabled unless configured. The sink uses a queued UDP client so
metric delivery does not block rate-limit checks:

```yaml
metrics:
  statsd:
    address: statsd:8125
    prefix: steward
    queue_capacity: 1024
```

The service emits request totals, allowed/over-limit counts, configuration
reload/error counts, Redis errors and operation latency, plus the observed
rate-limit value.
