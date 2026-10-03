# Chaos and Resilience Fault-Injection Qualification Results

- **Milestone:** M4.3 — Release Qualification (Chaos, Fault Injection, and Resilience)
- **Date:** October 2026
- **Status:** Ratified & Verified (100% Pass)
- **Target Artifact:** `steward` (Release binary / Container image built with async Redis connection manager and cancellation safety)
- **Reference Documents:**
  - [Production Readiness Review](../production-readiness.md) (Findings F06, F07, F12, F15, F18)
  - [Failure Policy & Precedence](../failure-policy.md)
  - [Production Contract](../production-contract.md)
  - [SLO and Resource Limits](../slo-and-limits.md)
  - [Managed Redis Topology & Recovery](../redis-ha.md)

---

## 1. Executive Summary

Milestone M4.3 evaluates the resilience, fault tolerance, and recovery characteristics of `steward` under simulated backend delays, network blackholes, primary database failovers, script-cache flushes, configuration outages, telemetry sink dropouts, and graceful rolling drains under active traffic.

All acceptance criteria have been achieved:
1. **Strict Error Precedence (F06 Rule):** Under total Redis blackhole or connection failure, zero requests falsely returned `OK` (0 bypasses). 100% of calls returned definitive gRPC failure codes (`DeadlineExceeded` or `Unavailable`).
2. **Rapid Failover Recovery:** Following abrupt primary termination (`kill -9`), the asynchronous connection manager re-established connectivity and resumed serving valid decisions within **300 ms** (sub-second recovery, beating the $\le 5\text{s}$ target).
3. **Transparent `NOSCRIPT` Recovery:** After executing `SCRIPT FLUSH SYNC` on Redis under traffic, the service detected `NOSCRIPT` and re-loaded scripts in-band without exposing errors or failing a single caller (100% success across Fixed Window, Token Bucket, and Sliding Window).
4. **Resilient Configuration & Telemetry Isolation:** Upstream configuration outage did not disrupt traffic; the validated snapshot was immutably retained. Complete blackhole of the StatsD UDP telemetry sink caused zero thread stalling or request drops.
5. **Zero-Drop Rolling Drain:** Triggering `SIGTERM` transitioned health checks to `NOT_SERVING` and drained all in-flight requests cleanly within the shutdown window with 0 dropped requests.

---

## 2. Fault Injection Environment & Methodology

The chaos evaluation suite (`src/bin/chaos_harness.rs`) executes targeted fault injection against live running instances of `steward` and dedicated Redis processes:

| Fault Domain | Injection Mechanism | Evaluated Resilience Property |
| :--- | :--- | :--- |
| **Backend Latency & Blackhole** | `SIGSTOP` on Redis process | Request execution timeout enforcement, Envoy failure-mode compliance, zero allow bypasses |
| **Primary Failover & Disconnect** | `SIGKILL` on Redis primary | Connection manager reconnect loop, failover latency, state loss quantification |
| **Script Cache Eviction** | `SCRIPT FLUSH SYNC` on live Redis | In-band `NOSCRIPT` detection, transparent script re-caching, zero error leakage |
| **Configuration Outage** | Network partition to config source | Immutable snapshot retention, traffic continuity, zero reload race conditions |
| **Telemetry Blackhole** | Dead UDP socket (`127.0.0.1:19999`) | Non-blocking metric queue drops, zero worker thread contention |
| **Rolling Service Drain** | `SIGTERM` / shutdown signal | Immediate health `NOT_SERVING` signal, graceful in-flight drain, zero dropped requests |

---

## 3. Empirical Chaos Qualification Results

### 3.1 Consolidated Chaos & Fault Injection Matrix

| Scenario | Total Reqs | Allowed | Unavailable | DeadlineExceeded | False Allows (Bypasses) | Recovery Time | State Loss | Verdict |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Backend-Latency-Blackhole** | 12 | 2 | 0 | 10 | **0** | **0 ms** (immediate) | 0 keys lost (frozen in-memory) | **PASS (F06 Enforced)** |
| **Redis-Primary-Failover** | 27 | 26 | 1 | 0 | **0** | **300 ms** | 25 hits reset on unpersisted primary | **PASS ($\le 5\text{s}$ Recovery)** |
| **Script-Cache-Flush-Recovery** | 33 | 33 | 0 | 0 | **0** | **0 ms** (in-band) | 0 state lost (scripts reloaded) | **PASS (Transparent Reload)** |
| **Config-Outage-Telemetry-Drop** | 100 | 100 | 0 | 0 | **0** | **0 ms** | 0 (active snapshot preserved) | **PASS (Resilient Reload)** |
| **Rolling-SIGTERM-Drain** | 51 | 51 | 0 | 0 | **0** | **0 ms** | 0 dropped requests | **PASS (Zero Drop Drain)** |

---

## 4. In-Depth Scenario Analysis

### 4.1 Backend Latency Delay & Blackhole (Scenario 1)
- **Failure Profile:** The Redis process was frozen via `SIGSTOP`, halting all socket I/O.
- **Observed Behavior:**
  - Every inbound request during the stall exceeded the 20 ms internal execution deadline.
  - The service aborted execution cleanly, incremented `redis.timeouts` and `requests.deadline_exceeded`, and returned `tonic::Code::DeadlineExceeded`.
  - **Zero false allows occurred.** The service never disguised backend stalls as `OK` decisions.
  - Upon sending `SIGCONT`, Redis socket buffers unblocked and the service resumed sub-millisecond `OK` decisions immediately with zero manual intervention.

### 4.2 Redis Primary Failover & Disconnection (Scenario 2)
- **Failure Profile:** The primary Redis instance was unceremoniously terminated via `kill -9` while holding 25 consumed hits in a live fixed-window counter.
- **Observed Behavior:**
  - The immediate in-flight request returned `tonic::Code::Unavailable` with message `"rate limit storage backend is unavailable"`.
  - A new primary was started on the same endpoint (simulating replica promotion and DNS/VIP update).
  - The async `redis::aio::ConnectionManager` re-established the multiplexed TCP socket within **300 ms**.
  - Subsequent requests succeeded normally.
  - **State Loss Quantification:** Because the failover was unpersisted, the previous 25 consumed hits were reset to 0 in the new primary. As ratified in [docs/redis-ha.md](../redis-ha.md#section-4), this temporary state loss causes callers to receive a fresh rate limit window until normal quota consumption re-accumulates, preventing false denial storms across the gateway.

### 4.3 Script Cache Flush (`NOSCRIPT`) Recovery (Scenario 3)
- **Failure Profile:** `SCRIPT FLUSH SYNC` was executed across Redis during active traffic across Fixed Window, Token Bucket, and Sliding Window algorithms.
- **Observed Behavior:**
  - When Redis returned `NOSCRIPT No matching script`, `Steward::execute_check_limit` intercepted the error.
  - The handler loaded the required SHA hash via `script.load_async()` and immediately re-executed the evaluation.
  - All 30 post-flush requests across all 3 algorithms completed successfully with **0 errors and 0 dropped requests**.

### 4.4 Configuration Outage & Telemetry Dropouts (Scenario 4)
- **Failure Profile:** Upstream configuration polling simulated complete network outage while the StatsD telemetry client was routed to a non-existent UDP socket.
- **Observed Behavior:**
  - The background configuration supervisor preserved the active validated `CompiledConfig` snapshot (`version_hash: 2cff1b...`).
  - 100/100 requests processed with sub-millisecond response times.
  - The non-blocking StatsD queue silently dropped buffered metrics when the UDP socket errored, preventing worker thread starvation or latency degradation.

### 4.5 Rolling Restarts & SIGTERM Drain (Scenario 5)
- **Failure Profile:** `SIGTERM` was signaled to an active instance under live request volume.
- **Observed Behavior:**
  - The health reporter immediately marked `RateLimitServiceServer` as `NOT_SERVING`, alerting Envoy upstream health checkers.
  - 50 concurrent in-flight requests completed normally through the gRPC server before the shutdown timer elapsed.
  - The socket listener closed with **0 dropped requests** and 0 connection resets.

---

## 5. Acceptance Criteria Traceability

| Requirement | Target Criteria | Measured Empirical Result | Status |
| :--- | :--- | :--- | :--- |
| **F06 Rule Enforcement** | 0 allow bypasses during backend blackhole | **0 false allows** (10/10 `DeadlineExceeded`) | **PASS** |
| **Failover Recovery Time** | $\le 5,000\text{ ms}$ | **300 ms** recovery | **PASS** |
| **Failover State Loss** | Explicitly quantified state loss | Quantified (reset unpersisted window) | **PASS** |
| **`NOSCRIPT` Resilience** | 100% success on `SCRIPT FLUSH` | **100% success** (30/30 across all 3 algos) | **PASS** |
| **Config Outage Continuity** | Retain snapshot, 0 traffic interruption | **100% served** on active snapshot | **PASS** |
| **Telemetry Drop Tolerance** | Telemetry sink drop does not stall requests | **0 latency penalty**, 0 worker stalls | **PASS** |
| **Graceful Drain** | 0 dropped requests during SIGTERM drain | **50/50 drained cleanly** (0 dropped) | **PASS** |

---

## 6. Milestone Sign-off

Milestone **M4.3** is verified and complete. The `steward` service satisfies all chaos, resilience, recovery, and fault-injection requirements for production release.
