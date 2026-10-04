# Runbook: Redis Failover, Out-of-Memory, and Connectivity

Use this runbook when Redis errors elevate, memory approaches limits, or a failover occurs.

---

## 1. Context

Steward uses Redis with `maxmemory-policy noeviction`. If Redis memory fills up:
- Write commands return an out-of-memory error (`OOM command not allowed`).
- Steward returns a gRPC `Unavailable` status.
- Envoy handles this according to its `failure_mode_deny` setting (`false` = admit traffic; `true` = reject traffic).

Steward uses an asynchronous connection manager that automatically reconnects when sockets drop and reloads Lua scripts on `NOSCRIPT` errors.

---

## 2. Diagnosis

### Check Memory Usage
```bash
redis-cli -h <redis-endpoint> -p 6379 INFO memory
```
Key fields:
- `used_memory_human`: Memory currently used.
- `maxmemory_human`: Configured ceiling.

### Test for OOM Errors
```bash
redis-cli -h <redis-endpoint> -p 6379 SET _steward_probe 1
```
If this returns `OOM command not allowed`, Redis has reached its memory limit.

### Check Replication Status
```bash
redis-cli -h <redis-endpoint> -p 6379 INFO replication
# Verify role is master and connected_slaves >= 1
```

---

## 3. Remediation

### Scenario A: Redis Memory Saturation (> 85% or OOM)
**Cause:** Active key count exceeded provisioned memory, often due to high-cardinality keys (e.g. unique user tokens) or large sliding-window sets.
**Actions:**
1. **Temporarily increase memory** if host capacity allows:
   ```bash
   redis-cli -h <redis-endpoint> CONFIG SET maxmemory <new_bytes>
   ```
2. **Scale up Redis instance size** via your cloud provider or infrastructure manager.
3. Review rate-limit policies to ensure descriptors do not use unbounded high-cardinality keys.

### Scenario B: Redis Primary Failover / Crash
**Cause:** Host reboot, hardware failure, or network disruption.
**Behavior:**
1. Standby replica is promoted to primary by Sentinel or managed cloud orchestrator.
2. Steward automatically re-establishes connections and re-caches Lua scripts on the new primary.
3. During the brief cutover, Envoy's `failure_mode_deny` setting determines whether traffic is admitted (default: fail-open) or rejected.
**Actions:**
- Monitor `redis.errors` in Grafana to confirm reconnection.
- Verify that DNS or VIP points to the new primary node.

### Scenario C: Replica Disconnected
**Cause:** Replica crashed or lagged behind the primary replication stream.
**Actions:**
1. Check replica container/host logs for crash or out-of-memory signals.
2. Restart or reprovision the replica to restore redundancy.

---

## 4. Verification
1. `redis.errors` and `redis.timeouts` drop back to 0.
2. Memory usage drops below 75% of `maxmemory`.
3. Canary write test succeeds:
   ```bash
   redis-cli -h <redis-endpoint> SET _steward_probe ok EX 10
   ```
