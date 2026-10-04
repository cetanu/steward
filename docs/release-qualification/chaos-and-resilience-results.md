# Chaos and Resilience Fault-Injection Qualification Results

- **Milestone:** M4.3 — Release Qualification (Chaos, Fault Injection, and Resilience)
- **Target Artifact:** `steward` (Release binary)
- **Reference Documents:**
  - [Production Readiness Review](../production-readiness.md) (Findings F06, F07, F12, F15, F18)
  - [Redis Operations Guide](../redis-operational-guide.md)
  - [Operational Runbooks](../runbooks/README.md)

---

## 1. Executive Summary

This document records the resilience, fault tolerance, and recovery test results for `steward` under simulated backend delays, network blackholes, database failovers, script-cache flushes, configuration outages, telemetry dropouts, and graceful rolling drains under active traffic.

Key results:
1. **Strict Error Precedence (F06 Rule):** Under total Redis blackhole or connection failure, zero requests returned `OK` (0 false allows). 100% of calls returned definitive gRPC failure codes (`DeadlineExceeded` or `Unavailable`).
2. **Failover Recovery:** Following abrupt primary termination (`SIGKILL`), the asynchronous connection manager re-established connectivity and resumed serving decisions within **300 ms** (well within the <= 5s target).
3. **Transparent `NOSCRIPT` Recovery:** After executing `SCRIPT FLUSH SYNC` on Redis under traffic, the service detected `NOSCRIPT` and re-loaded scripts in-band without exposing errors or failing a caller (100% success across Fixed Window, Token Bucket, and Sliding Window).
4. **Configuration & Telemetry Resilience:** Upstream configuration outage did not disrupt traffic; the validated snapshot was immutably retained. Complete blackhole of the StatsD UDP telemetry sink caused zero thread stalling or request drops.
5. **Graceful Drain:** Triggering `SIGTERM` transitioned health checks to `NOT_SERVING` and drained all in-flight requests cleanly within the shutdown window with 0 dropped requests.

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

## 3. Results Matrix

| Scenario | Total Reqs | Allowed | Unavailable | DeadlineExceeded | False Allows (Bypasses) | Recovery Time | State Loss | Status |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Backend-Latency-Blackhole** | 12 | 2 | 0 | 10 | **0** | **0 ms** (immediate) | 0 keys lost (frozen in-memory) | **PASS (F06 Enforced)** |
| **Redis-Primary-Failover** | 27 | 26 | 1 | 0 | **0** | **300 ms** | 25 hits reset on unpersisted primary | **PASS (<= 5s Recovery)** |
| **Script-Cache-Flush-Recovery** | 33 | 33 | 0 | 0 | **0** | **0 ms** (in-band) | 0 state lost (scripts reloaded) | **PASS (Transparent Reload)** |
| **Config-Outage-Telemetry-Drop** | 100 | 100 | 0 | 0 | **0** | **0 ms** | 0 (active snapshot preserved) | **PASS (Resilient Reload)** |
| **Rolling-SIGTERM-Drain** | 51 | 51 | 0 | 0 | **0** | **0 ms** | 0 dropped requests | **PASS (Zero Drop Drain)** |

---

## 4. Scenario Analysis

### 4.1 Backend Latency Delay & Blackhole
- **Failure Profile:** The Redis process was frozen via `SIGSTOP`, halting all socket I/O.
- **Observed Behavior:**
  - Inbound requests during the stall exceeded the 20 ms internal execution deadline.
  - The service aborted execution cleanly, incremented `redis.timeouts` and `requests.deadline_exceeded`, and returned `tonic::Code::DeadlineExceeded`.
  - Zero false allows occurred. The service never disguised backend stalls as `OK` decisions.
  - Upon sending `SIGCONT`, Redis socket buffers unblocked and the service resumed normal decisions immediately.

### 4.2 Redis Primary Failover & Disconnection
- **Failure Profile:** The primary Redis instance was terminated abruptly (`SIGKILL`) while holding 25 consumed hits in a live fixed-window counter.
- **Observed Behavior:**
  - The immediate in-flight request returned `tonic::Code::Unavailable` with message `"rate limit storage backend is unavailable"`.
  - A new primary was started on the same endpoint (simulating replica promotion).
  - The async `redis::aio::ConnectionManager` re-established the socket within **300 ms**.
  - Subsequent requests succeeded normally.
  - State loss: because the failover was unpersisted, the previous 25 consumed hits were reset to 0 in the new primary. Callers received a fresh rate limit window until normal quota consumption re-accumulated.

### 4.3 Script Cache Flush (`NOSCRIPT`) Recovery
- **Failure Profile:** `SCRIPT FLUSH SYNC` was executed across Redis during active traffic across Fixed Window, Token Bucket, and Sliding Window algorithms.
- **Observed Behavior:**
  - When Redis returned `NOSCRIPT No matching script`, `Steward::execute_check_limit` intercepted the error.
  - The handler loaded the required SHA hash via `script.load_async()` and immediately retried the evaluation.
  - All 30 post-flush requests across all 3 algorithms completed successfully with zero errors.

### 4.4 Configuration Outage & Telemetry Dropouts
- **Failure Profile:** Upstream configuration polling simulated network outage while the StatsD telemetry client was routed to an unavailable UDP socket.
- **Observed Behavior:**
  - The background configuration supervisor preserved the active validated `CompiledConfig` snapshot.
  - 100/100 requests processed normally.
  - The non-blocking StatsD queue dropped buffered metrics when the UDP socket errored, preventing worker thread starvation or latency degradation.

### 4.5 Rolling Restarts & SIGTERM Drain
- **Failure Profile:** `SIGTERM` was sent to an active instance under live request volume.
- **Observed Behavior:**
  - The health reporter immediately marked `RateLimitServiceServer` as `NOT_SERVING`, alerting Envoy upstream health checkers.
  - 50 concurrent in-flight requests completed normally before the shutdown timer elapsed.
  - The socket listener closed with 0 dropped requests and 0 connection resets.

---

## 5. Acceptance Summary

| Requirement | Target Criteria | Measured Result | Status |
| :--- | :--- | :--- | :--- |
| **F06 Rule Enforcement** | 0 allow bypasses during backend blackhole | **0 false allows** (10/10 `DeadlineExceeded`) | PASS |
| **Failover Recovery Time** | <= 5,000 ms | **300 ms** recovery | PASS |
| **Failover State Loss** | Quantified state loss | Quantified (reset unpersisted window) | PASS |
| **`NOSCRIPT` Resilience** | 100% success on `SCRIPT FLUSH` | **100% success** (30/30 across all 3 algos) | PASS |
| **Config Outage Continuity** | Retain snapshot, 0 traffic interruption | **100% served** on active snapshot | PASS |
| **Telemetry Drop Tolerance** | Telemetry sink drop does not stall requests | **0 latency penalty**, 0 worker stalls | PASS |
| **Graceful Drain** | 0 dropped requests during SIGTERM drain | **50/50 drained cleanly** (0 dropped) | PASS |
