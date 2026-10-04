# Canary Rollout and Automated Verification Report

- **Milestone:** M4.4 — Release Qualification (Canary Rollout & Automated Stop Conditions)
- **Target Artifact:** `steward` (Release candidate binary)
- **Reference Documents:**
  - [Production Readiness Review](../production-readiness.md) (Milestone M4.4)
  - [Operational Runbooks](../runbooks/README.md)

---

## 1. Executive Summary

Milestone M4.4 executes an automated canary rollout and verification procedure to validate the candidate release artifact in a deployment alongside existing baseline instances.

Traffic was distributed using a **90% Baseline / 10% Canary split** coupled with continuous shadow dual-evaluation to detect policy divergence, latency regressions, resource anomalies, and enforcement bypasses. Quota window transitions were observed across active boundaries.

All automated stop condition gates evaluated to **PASS**. Zero abort conditions were triggered.

---

## 2. Predefined Automated Stop Conditions

In accordance with [docs/production-readiness.md](../production-readiness.md#m4-tasks--qualify-and-release), the deployment orchestrator monitored the canary workload against quantitative stop conditions:

| Stop Condition Rule | Abort Threshold | Action if Breached |
| :--- | :--- | :--- |
| **Latency Degradation (p99)** | > +10.0% increase relative to baseline | Drain canary traffic, revert to baseline |
| **Enforcement Error Rate** | > 0.01% gRPC errors (`Unavailable`, etc.) | Drain canary traffic, revert to baseline |
| **Bypass / False Allow Rate** | > 0.00% (any false `OK` on backend failure) | Critical alert, isolate canary, abort |
| **Policy Divergence Rate** | > 0.00% decision delta on identical inputs | Drain canary traffic, freeze version |
| **Redis Saturation / CPU Anomaly** | CPU > 70% or RSS > 512 MiB | Revert to baseline |

---

## 3. Deployment & Verification Topology

The canary verification suite (`src/bin/canary_harness.rs`) deployed two isolated service instances sharing a common Redis primary:

| Component | Baseline Deployment | Canary Candidate Deployment |
| :--- | :--- | :--- |
| **Service Endpoint** | `127.0.0.1:52051` | `127.0.0.1:52052` |
| **Traffic Weight** | 90% (9,000 requests) | 10% (1,000 requests) + Shadow Dual-Eval |
| **Config Version Hash** | `8040a1e0f3681fbea7cf...` | `8040a1e0f3681fbea7cf...` (Agreement) |
| **Execution Timeout** | 100 ms | 100 ms |
| **Target Policies** | Fixed Window (`standard`), Token Bucket (`enterprise`) | Fixed Window (`standard`), Token Bucket (`enterprise`) |

---

## 4. Canary Results

### 4.1 Gate Evaluation Matrix

| Verification Gate | Predefined Threshold | Observed Value | Status |
| :--- | :--- | :--- | :--- |
| **Latency Degradation (p99)** | <= +10.0% | **-2.56%** (no degradation) | PASS |
| **Enforcement Error Rate** | <= 0.01% | **0.0000%** (0 errors / 1,000 canary reqs) | PASS |
| **Bypass / False Allow Rate** | 0.00% | **0.0000%** (0 false allows) | PASS |
| **Policy Divergence Rate** | 0.00% | **0.0000%** (0 divergent decisions) | PASS |
| **Window Rollover Correctness** | Boundary reset verified | 50 allow -> 5 deny -> boundary reset -> OK | PASS |
| **Resource Stability** | RSS <= 512 MiB | Baseline: 35.8 MiB \| Canary: 35.9 MiB | PASS |

### 4.2 Latency Percentile Comparison

| Metric | Baseline Instance (`:52051`) | Canary Candidate (`:52052`) | Variance / Delta |
| :--- | :--- | :--- | :--- |
| **p50 Latency** | 0.04 ms | 0.04 ms | 0.0% |
| **p95 Latency** | 0.06 ms | 0.06 ms | 0.0% |
| **p99 Latency** | 0.08 ms | 0.08 ms | **-2.56%** |

### 4.3 Quota Window Duration & Boundary Observation
- **Observation:** Quota consumption for key `standard:boundary_client` was exercised to exhaustion (50 allowed calls, followed by 5 denied calls).
- **Boundary Transition:** The test harness waited across the fixed-window 1-second boundary (1.1s).
- **Verification:** The request immediately following the boundary transition returned `OK`, validating clean calendar-aligned window rollover.

---

## 5. Rollout Procedure

With all predefined stop conditions satisfied and zero policy divergences observed, the candidate release artifact meets the requirements of Milestone M4.4. Rollout and rollback procedures are documented in [`docs/runbooks/README.md`](../runbooks/README.md).
