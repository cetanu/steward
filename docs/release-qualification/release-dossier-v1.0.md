# Production Release Dossier — Steward v0.1.0

- **Release Version:** `v0.1.0`
- **Target Artifact:** `ghcr.io/cetanu/steward:latest`
- **Reference Documentation:**
  - [Production Readiness Review](../production-readiness.md)
  - [Algorithms](../algorithms.md)
  - [Redis Operations Guide](../redis-operational-guide.md)
  - [Performance Benchmarks](../benchmarks.md)
  - [Operational Runbooks](../runbooks/README.md)
  - [M4.1 Correctness Matrix Results](correctness-matrix-results.md)
  - [M4.2 Load & Soak Results](load-and-soak-results.md)
  - [M4.3 Chaos & Resilience Results](chaos-and-resilience-results.md)
  - [M4.4 Canary Verification Report](canary-verification.md)

---

## 1. Summary

Steward is an asynchronous Envoy Rate Limit Service (RLS) written in Rust. It enforces rate limits across upstream APIs and microservices with ordered descriptor matching, deterministic error precedence, admission control, and storage in Redis.

This document summarizes qualification evidence verifying that Steward v0.1.0 satisfies the architectural goals, service level targets, and operational requirements outlined in [docs/production-readiness.md](../production-readiness.md). All 18 findings (F01–F18) are addressed and verified.

---

## 2. Build & Packaging

| Attribute | Specification |
| :--- | :--- |
| **Artifact Image** | `ghcr.io/cetanu/steward:latest` |
| **Compiler Toolchain** | Rust 1.88 (`x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu`) |
| **Base Operating System** | Ubuntu 24.04 LTS (`noble`) |
| **Security Context** | Non-root UID `10001:10001`, `read_only_rootfs: true`, `no-new-privileges` |
| **Crypto Provider** | `aws-lc-rs` |
| **Protobuf Closure** | Pinned Envoy v1.39.0 RLS closure vendored in repository under `proto/` |

---

## 3. Verified Operating Envelope

| Parameter | Target Budget | Measured Result | Status |
| :--- | :--- | :--- | :--- |
| **Supported Workload** | 20,000 QPS (4 vCPU / 4 GiB) | **29,994 QPS completed** | PASS |
| **Decision Latency (p50)** | <= 1.0 ms | **0.90 ms** (Fixed Window) / **0.04 ms** (Canary) | PASS |
| **Decision Latency (p95)** | <= 2.5 ms | **1.47 ms** | PASS |
| **Decision Latency (p99)** | <= 5.0 ms | **1.64 ms** | PASS |
| **Decision Latency (p99.9)** | <= 10.0 ms | **4.87 ms** | PASS |
| **3x Traffic Burst** | 60,000 QPS spike | **59,978 QPS completed** at p99 = 3.22 ms | PASS |
| **High-Cardinality Churn** | 100,000 unique client keys | Stable **35.9 MiB RSS** | PASS |
| **Service RSS Ceiling** | <= 512 MiB | **36.5 MiB peak** | PASS |
| **Service CPU** | <= 70% at steady state | < 55% CPU utilization under 20k QPS | PASS |
| **Admission Limit** | 4,096 in-flight permits | Non-blocking load shedding with immediate `ResourceExhausted` | PASS |
| **Execution Deadline** | 20 ms internal timeout | Strict cancellation safety, zero hanging calls | PASS |

---

## 4. Findings Closure Matrix

| ID | Priority | Area | Summary of Resolution | Status |
| :--- | :--- | :--- | :--- | :--- |
| **F01** | P0 | Matching | Trie-based matching over ordered descriptors with deterministic exact over wildcard precedence. Dynamic wildcard value captured in counter key identity. | CLOSED |
| **F02** | P0 | Protocol | Input bounds (16 descriptors, 8 entries, 256B strings). Validated descriptor-level `hits_addend` precedence, zero-cost read-only probes, negative hit refunds, and override validation. | CLOSED |
| **F03** | P0 | Algorithms | Replaced application client clocks with authoritative Redis `TIME`. Added 128-bit nonces to sliding window events to prevent timestamp collision. | CLOSED |
| **F04** | P0 | Resources | Capped sliding window retention at 10,000 events via `ZREMRANGEBYRANK` pruning. Capped hit cost at 100 to prevent Redis script stalls. | CLOSED |
| **F05** | P0 | Config | Gated startup: initial configuration snapshot is fetched, validated, and compiled before opening socket listener. Rejects non-positive capacities and unknown units. | CLOSED |
| **F06** | P0 | Failure | Implemented F06 Error Precedence: definitive quota rejection (`OVER_LIMIT`) always wins; storage backend errors/timeouts return `Unavailable`/`DeadlineExceeded`. Zero false allows. | CLOSED |
| **F07** | P0 | Overload | Global admission semaphore for non-blocking load shedding. Standard `grpc-timeout` header parsing with strict internal execution deadlines. | CLOSED |
| **F08** | P1 | Backend | Replaced sync `r2d2` pool with asynchronous multiplexed `redis::aio::ConnectionManager`. Eliminated `block_in_place`. Evaluates multiple rules concurrently via `join_all`. | CLOSED |
| **F09** | P1 | Protocol | Preserved 1:1 descriptor-to-status response mapping in input order. Populates `current_limit`, `limit_remaining`, and `duration_until_reset`. | CLOSED |
| **F10** | P1 | Algorithms | Models documented in `docs/algorithms.md`. Unit test certification suite verified against pure Rust reference models. | CLOSED |
| **F11** | P1 | State | Counter keys formatted independently of mutable capacity thresholds, preserving consumed quota during config updates. | CLOSED |
| **F12** | P1 | Config | Configuration supervisor retains immutable `Arc<CompiledConfig>` snapshot across reload failures. Emits loader health metrics. | CLOSED |
| **F13** | P1 | Build | Pinned, tracked `.proto` closure in repository. Removed network download build dependencies. | CLOSED |
| **F14** | P1 | Security | Non-root container (`UID 10001`), TLS transport with `aws-lc-rs`, authenticated Redis URL support (`rediss://`), secret sanitization in logs. | CLOSED |
| **F15** | P1 | Lifecycle | Integrated gRPC Health Checking Protocol (`TonicHealthService`). Bounded SIGTERM graceful drain: sets `NOT_SERVING`, finishes in-flight requests. | CLOSED |
| **F16** | P1 | Storage HA | Managed Redis primary/replica HA operational guidelines in `docs/redis-operational-guide.md`. Fast reconnect loop and quantified failover state loss. | CLOSED |
| **F17** | P1 | Gateway | Envoy HTTP/2 connection pooling, circuit breaking, request timeouts, and draft-03 rate limit headers. | CLOSED |
| **F18** | P1 | Operations | Prometheus alert rules, Grafana SLO dashboards, operational runbooks, and completed qualification suites (M4.1–M4.4). | CLOSED |

---

## 5. Qualification Suite Summary

| Phase | Scope | Key Evidence | Status |
| :--- | :--- | :--- | :--- |
| **M4.1: Correctness Matrix** | 93 unit tests, live Envoy v1.39 integration | 100% pass rate. Verified ordered matching, wildcards, hit weights, zero-cost probes, refunds, error precedence, and header propagation. | PASS |
| **M4.2: Load & Soak** | Offered load sweeps (5k–30k QPS), churn (100k keys), 3x burst (60k QPS), 6-hour soak | p99 = 1.64 ms at 20k QPS; linear scaling to 30k QPS; peak RSS 36.5 MiB; zero memory leaks. | PASS |
| **M4.3: Chaos & Resilience** | Redis blackholes (`SIGSTOP`), primary failover (`SIGKILL`), `SCRIPT FLUSH`, config/telemetry drops, SIGTERM drain | F06 0 bypasses; 300 ms failover recovery; transparent `NOSCRIPT` reload (100% success); 0 dropped requests during drain. | PASS |
| **M4.4: Canary Verification** | 90/10 traffic split, shadow dual-eval, stop conditions | 0.00% error rate, 0.00% policy divergence, -2.56% p99 delta, clean window boundary rollover. | PASS |

---

## 6. Operational Resources

- **Redis Operational Guide:** [`docs/redis-operational-guide.md`](../redis-operational-guide.md)
- **Monitoring & Alert Rules:** [`docs/monitoring/alerts/steward-alerts.yaml`](../monitoring/alerts/steward-alerts.yaml)
- **Grafana Dashboard:** [`docs/monitoring/dashboards/README.md`](../monitoring/dashboards/README.md)
- **Operational Runbooks:** [`docs/runbooks/README.md`](../runbooks/README.md)
  - [Latency Degradation, Deadlines, and Admission Shedding](../runbooks/latency-and-deadlines.md)
  - [Redis Primary Failover, Memory Exhaustion, and Connectivity](../runbooks/redis-failover-and-oom.md)
  - [Stale Configuration, Loader Failures, and Emergency Rollback](../runbooks/stale-config-and-rollback.md)
  - [Emergency Traffic Bypass & Load Shedding Procedures](../runbooks/emergency-bypass.md)
