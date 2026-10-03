# Production Release Dossier — Steward v0.1.0

- **Release Version:** `v0.1.0` (Production General Availability)
- **Date:** October 2026
- **Status:** **RATIFIED FOR PRODUCTION RELEASE (100% Qualification Pass)**
- **Target Artifact:** `ghcr.io/cetanu/steward:v0.1.0`
- **Reference Specifications:**
  - [Production Readiness Review](../production-readiness.md)
  - [Production Contract](../production-contract.md)
  - [SLO and Resource Limits](../slo-and-limits.md)
  - [Failure Policy & Precedence](../failure-policy.md)
  - [Accounting Contract](../accounting-contract.md)
  - [Algorithms Specification](../algorithms.md)
  - [Managed Redis Topology & Failover](../redis-ha.md)
  - [Envoy Client Tuning](../envoy-tuning.md)
  - [Dashboards and Alert Rules](../dashboards-and-alerts.md)
  - [Operational Runbooks](../runbooks.md)
  - [M4.1 Correctness Matrix Results](correctness-matrix-results.md)
  - [M4.2 Load & Soak Results](load-and-soak-results.md)
  - [M4.3 Chaos & Resilience Results](chaos-and-resilience-results.md)
  - [M4.4 Canary Verification Report](canary-verification.md)

---

## 1. Executive Summary & Readiness Declaration

Steward is an ultra-high-performance, asynchronous Envoy Rate Limit Service (RLS) built in Rust. It enforces rate limits across upstream APIs and microservices with deterministic matching, strict error precedence, non-blocking admission control, and authoritative storage in Redis.

This Release Dossier synthesizes the end-to-end qualification evidence demonstrating that **Steward v0.1.0 satisfies all architectural requirements, Service Level Objectives (SLOs), security gates, and operational criteria** set forth in [docs/production-readiness.md](../production-readiness.md). All 18 findings (P0 findings F01–F07 and P1 findings F08–F18) are verified closed.

**Readiness Verdict: APPROVED FOR PRODUCTION DEPLOYMENT.**

---

## 2. Artifact & Cryptographic Provenance

The production artifact is built and packaged through hardened, reproducible CI/CD pipelines:

| Attribute | Specification / Verification Value |
| :--- | :--- |
| **Artifact Image** | `ghcr.io/cetanu/steward:v0.1.0` / `latest` |
| **Compiler Toolchain** | Rust 1.88 (`x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu`) |
| **Base Operating System** | Ubuntu 24.04 LTS (`noble`), GLIBC 2.38 |
| **Security Context** | Non-root UID `10001:10001`, `read_only_rootfs: true`, `no-new-privileges` |
| **Crypto Provider** | `aws-lc-rs v1.18` (FIPS-capable, constant-time, zero `ring` dependency) |
| **Protobuf Closure** | Pinned Envoy v1.39.0 RLS closure tracked in repository (zero remote build downloads) |
| **Software Bill of Materials** | CycloneDX JSON SBOM generated during build (`cargo sbom`) |
| **Vulnerability Scanning** | Trivy and OSV container security scans: **0 Critical / 0 High CVEs** |

---

## 3. Verified Production Operating Envelope

Empirical benchmark and qualification results certify the following supported operating envelope:

| Parameter | Ratified Target / SLO Ceiling | Demonstrated Empirical Capability | Margin |
| :--- | :--- | :--- | :--- |
| **Supported Workload** | 20,000 QPS (4 vCPU / 4 GiB) | **29,994 QPS completed** (1.5× qualification target) | **+50%** |
| **Decision Latency ($p50$)** | $\le 1.0\text{ ms}$ | **0.90 ms** (Fixed Window) / **0.04 ms** (Canary) | **PASS** |
| **Decision Latency ($p95$)** | $\le 2.5\text{ ms}$ | **1.47 ms** | **PASS** |
| **Decision Latency ($p99$)** | $\le 5.0\text{ ms}$ | **1.64 ms** | **67% margin** |
| **Decision Latency ($p99.9$)** | $\le 10.0\text{ ms}$ | **4.87 ms** | **51% margin** |
| **3× Traffic Burst** | 60,000 QPS spike | **59,978 QPS completed** at $p99 = 3.22\text{ ms}$, 0 drops | **PASS** |
| **High-Cardinality Churn** | 100,000 unique client keys | Stable **35.9 MiB RSS**, zero unbounded growth | **PASS** |
| **Service RSS Ceiling** | $\le 512\text{ MiB}$ | **36.5 MiB peak** across 6-hour soak profile | **93% margin** |
| **Service CPU Ceiling** | $\le 70\%$ at steady state | $< 55\%$ CPU utilization under 20k QPS | **PASS** |
| **Admission Limits** | 4,096 in-flight permits | Strict load shedding with immediate `ResourceExhausted` | **PASS** |
| **Execution Deadline** | 20 ms internal timeout | 100% cancellation safety; 0 hanging calls during stalls | **PASS** |

---

## 4. Findings Closure Traceability Matrix

Every architectural finding identified in the Production Readiness Review has been formally resolved, tested, and certified:

| ID | Severity | Area | Resolution Summary | Milestone / PR | Status |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **F01** | **P0** | Matching | Hierarchical trie-based matching over ordered descriptors with deterministic exact over wildcard precedence. Dynamic wildcard value captured in counter key identity. | M1.2 (PR #38) | **CLOSED** |
| **F02** | **P0** | Protocol | Enforced input bounds (16 descriptors, 8 entries, 256B strings). Validated descriptor-level `hits_addend` precedence, zero-cost read-only probes, negative hit refunds, and override validation. | M1.4 (PR #40) | **CLOSED** |
| **F03** | **P0** | Algorithms | Replaced application client clocks with authoritative Redis `TIME`. Added 128-bit high-entropy nonces to sliding window events to prevent timestamp collision. | M2.3 (PR #45) | **CLOSED** |
| **F04** | **P0** | Resources | Capped sliding window retention at 10,000 events via `ZREMRANGEBYRANK` pruning. Capped hit cost at 100 to prevent Redis Lua script execution stalls. | M2.3 (PR #45) | **CLOSED** |
| **F05** | **P0** | Config | Gated startup: initial configuration snapshot must be fetched, validated, and compiled before opening socket listener. Rejects non-positive capacities and unknown units. | M1.3 (PR #39) | **CLOSED** |
| **F06** | **P0** | Failure | Implemented F06 Error Precedence: definitive quota rejection (`OVER_LIMIT`) always wins; storage backend errors/timeouts return `Unavailable`/`DeadlineExceeded`. Zero false allows. | M1.5 (PR #41) | **CLOSED** |
| **F07** | **P0** | Overload | Global admission semaphore (4,096 permits) for non-blocking load shedding. Standard `grpc-timeout` header parsing with strict internal execution deadlines (20 ms). | M2.2 (PR #44) | **CLOSED** |
| **F08** | **P1** | Backend | Replaced sync `r2d2` pool with asynchronous multiplexed `redis::aio::ConnectionManager`. Eliminated `block_in_place`. Evaluates multiple rules concurrently via `join_all`. | M2.1 & M2.5 (PRs #43, #47) | **CLOSED** |
| **F09** | **P1** | Protocol | Preserved strict 1:1 descriptor-to-status response mapping in input order. Populates `current_limit`, `limit_remaining`, and `duration_until_reset`. | M1.5 (PR #41) | **CLOSED** |
| **F10** | **P1** | Algorithms | Mathematical models documented in `docs/algorithms.md`. Unit test certification suite verified against pure Rust reference models. | M2.4 (PR #46) | **CLOSED** |
| **F11** | **P1** | State | Canonical Redis counter keys formatted independently of mutable capacity thresholds, preserving consumed quota during live config edits and overrides. | M1.4 & M2.4 (PRs #40, #46) | **CLOSED** |
| **F12** | **P1** | Config | Configuration supervisor retains immutable `Arc<CompiledConfig>` snapshot across reload failures. Emits loader health metrics (`consecutive_failures`, `config_age_seconds`). | M1.3 & M3.4 (PRs #39, #54) | **CLOSED** |
| **F13** | **P1** | Supply Chain | Replaced remote build-time HTTP downloads with minimal, pinned, tracked `.proto` closure in repository. Removed `reqwest` and `zip` build dependencies. | M1.1 (PR #37) | **CLOSED** |
| **F14** | **P1** | Security | Multi-stage non-root container (`UID 10001`), TLS transport with `aws-lc-rs`, authenticated Redis URL support (`rediss://`), secret sanitization in logs. | M3.1 & M3.2 (PRs #51, #52) | **CLOSED** |
| **F15** | **P1** | Lifecycle | Integrated gRPC Health Checking Protocol (`TonicHealthService`). Bounded SIGTERM graceful drain: sets `NOT_SERVING`, finishes in-flight requests, zero drops. | M3.3 (PR #53) | **CLOSED** |
| **F16** | **P1** | Storage HA | Declared managed Redis primary/replica HA with Sentinel / AWS ElastiCache / GCP Memorystore. Reconnect loop resumes in $< 300\text{ ms}$. Quantified failover state loss. | M3.5 (PR #55) | **CLOSED** |
| **F17** | **P1** | Gateway | Published explicit Envoy tuning guides: HTTP/2 connection pooling, circuit breaking, request timeouts, and draft-03 rate limit headers. | M3.6 (PR #56) | **CLOSED** |
| **F18** | **P1** | Operations | Published Prometheus alert rules, Grafana SLO dashboards, operational runbooks, and completed qualification suites (M4.1–M4.4). | M3.7 & M4.1–M4.4 (PRs #57–#62) | **CLOSED** |

---

## 5. Qualification Suite Summary

| Phase | Evaluation Scope | Key Findings & Evidence | Status |
| :--- | :--- | :--- | :--- |
| **M4.1: Correctness Matrix** | 93 automated unit tests, end-to-end Envoy v1.39 integration | 100% pass rate. Verified ordered matching, wildcards, hit weights, zero-cost probes, refunds, error precedence, and header propagation. | **PASS** |
| **M4.2: Load & Soak** | Offered load sweeps (5k–30k QPS), churn (100k keys), 3× burst (60k QPS), 6-hour soak | $p99 = 1.64\text{ ms}$ at 20k QPS; linear scaling to 30k QPS; peak RSS 36.5 MiB; zero memory leaks. | **PASS** |
| **M4.3: Chaos & Resilience** | Redis blackholes (`SIGSTOP`), primary failover (`SIGKILL`), `SCRIPT FLUSH`, config/telemetry drops, SIGTERM drain | F06 0 bypasses; 300 ms failover recovery; transparent `NOSCRIPT` reload (100% success); 0 dropped requests during drain. | **PASS** |
| **M4.4: Canary Verification** | 90/10 traffic split, shadow dual-eval, stop conditions | 0.00% error rate, 0.00% policy divergence, -2.56% $p99$ delta, clean window boundary rollover. Approved for 100% promotion. | **PASS** |

---

## 6. Operational Handover & Support

### 6.1 Documentation & Runbook Assets
- **Deployment & Operating Contract:** [docs/production-contract.md](../production-contract.md)
- **Monitoring & Alert Rules:** [docs/dashboards-and-alerts.md](../dashboards-and-alerts.md)
- **Operational Runbooks:** [docs/runbooks.md](../runbooks.md) covering:
  - High Admission Load Shedding (`StewardAdmissionLoadShedding`)
  - Elevated Decision Latency (`StewardDecisionLatencyHigh`)
  - Stale Configuration Snapshot (`StewardConfigurationStale`)
  - Storage Backend Unavailability (`StewardRedisBackendUnavailable`)
  - Emergency Policy Rollback & Canary Abort

### 6.2 Service Ownership
- **Tier 1 Support:** Enterprise Site Reliability Engineering (SRE) / 24x7 Operations.
- **Tier 2 Support:** API Gateway & Core Platform Engineering.
- **Escalation Path:** PagerDuty schedule `steward-service-oncall`.

---

## 7. Formal Release Sign-Off

The undersigned certify that Steward v0.1.0 has fulfilled all release qualification criteria and is ready for production service:

- **Lead Systems Engineer:** Vasilios Syrakis (`syrakis@pm.me`)
- **Gateway Platform SRE Lead:** Ratified
- **Release Qualification Status:** **APPROVED FOR GENERAL AVAILABILITY (GA)**
