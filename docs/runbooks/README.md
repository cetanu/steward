# Steward Operational Runbooks

This directory contains actionable operational runbooks for engineers and SREs maintaining Steward rate-limit service clusters in production.

## Incident Triage Matrix

| Alert / Symptom | Severity | Primary Runbook | Immediate Action |
| :--- | :--- | :--- | :--- |
| **`StewardEnforcementSLOHighBurnRate`** | Critical | [`latency-and-deadlines.md`](file:///home/vsyrakis/Documents/steward/docs/runbooks/latency-and-deadlines.md) | Check Redis latency, error rates, and admission load shedding. |
| **`StewardAdmissionLoadSheddingActive`** | Critical | [`latency-and-deadlines.md`](file:///home/vsyrakis/Documents/steward/docs/runbooks/latency-and-deadlines.md) | Concurrency bottleneck: scale Steward replicas or inspect Redis slowlogs. |
| **`StewardRequestDeadlinesExceeded`** | Critical | [`latency-and-deadlines.md`](file:///home/vsyrakis/Documents/steward/docs/runbooks/latency-and-deadlines.md) | Execution time > 10ms: check Redis CPU saturation and network RTT. |
| **`StewardRedisErrorRateElevated`** | Critical | [`redis-failover-and-oom.md`](file:///home/vsyrakis/Documents/steward/docs/runbooks/redis-failover-and-oom.md) | Verify Redis primary connectivity, OOM rejections, or failover status. |
| **`StewardRedisMemorySaturationCritical`** | Critical | [`redis-failover-and-oom.md`](file:///home/vsyrakis/Documents/steward/docs/runbooks/redis-failover-and-oom.md) | Scale up Redis memory or truncate active sliding-window rules. |
| **`StewardConfigurationStaleCritical`** | Critical | [`stale-config-and-rollback.md`](file:///home/vsyrakis/Documents/steward/docs/runbooks/stale-config-and-rollback.md) | Policy age > 1 hour: check config distribution URL, syntax, and permissions. |
| **`StewardConfigurationFleetVersionDivergence`** | Warning | [`stale-config-and-rollback.md`](file:///home/vsyrakis/Documents/steward/docs/runbooks/stale-config-and-rollback.md) | Check if a rolling deployment stalled or if a subset of replicas is failing to fetch. |
| **Total Ingress Blockage / Upstream Emergency** | Catastrophic | [`emergency-bypass.md`](file:///home/vsyrakis/Documents/steward/docs/runbooks/emergency-bypass.md) | Toggle Envoy `failure_mode_deny: false` or disable RLS filter in Envoy route. |

---

## Runbook Directory

1. [Latency Degradation, Deadlines, and Admission Shedding](file:///home/vsyrakis/Documents/steward/docs/runbooks/latency-and-deadlines.md)
2. [Redis Primary Failover, Memory Exhaustion, and Connectivity](file:///home/vsyrakis/Documents/steward/docs/runbooks/redis-failover-and-oom.md)
3. [Stale Configuration, Loader Failures, and Emergency Rollback](file:///home/vsyrakis/Documents/steward/docs/runbooks/stale-config-and-rollback.md)
4. [Emergency Traffic Bypass & Load Shedding Procedures](file:///home/vsyrakis/Documents/steward/docs/runbooks/emergency-bypass.md)
