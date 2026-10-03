# Runbook: Redis Primary Failover, Memory Exhaustion, and Connectivity

**Target Alerts:**
- `StewardRedisErrorRateElevated`
- `StewardRedisMemorySaturationWarning`
- `StewardRedisMemorySaturationCritical`
- `StewardRedisReplicaDisconnected`

---

## 1. Overview & Architecture Context

Steward relies on Redis as its authoritative external counter store. To protect rate-limiting semantics against silent quota resets, all production Redis instances enforce **`maxmemory-policy noeviction`** as ratified in [`docs/redis-operational-guide.md`](file:///home/vsyrakis/Documents/steward/docs/redis-operational-guide.md).

When Redis reaches `maxmemory`, it rejects write operations with `OOM command not allowed when used memory > 'maxmemory'`. Steward catches this and applies the **F06 Error Precedence Rule**:
- Definitively denied calls (`OVER_LIMIT`) continue to be enforced.
- Undetermined calls return gRPC `Status::unavailable("rate limit storage backend is unavailable")`.
- Envoy intercepts `Unavailable` and enforces its configured `failure_mode_deny` posture.

Steward multiplexes requests over an async `redis::aio::ConnectionManager`, automatically reconnecting upon socket termination and reloading Lua scripts upon `NOSCRIPT` cache flushes.

---

## 2. Immediate Diagnostic Steps

### Step 1: Connect to Redis and Inspect Memory
```bash
# Connect to current Redis endpoint
redis-cli -h <redis-endpoint> -p 6379 INFO memory
```
Key metrics to review:
- `used_memory_human`: Total memory allocated by Redis.
- `maxmemory_human`: Maximum memory threshold.
- `mem_fragmentation_ratio`: If $> 1.5$, memory allocator fragmentation is elevated.

### Step 2: Check for OOM Errors and Rejections
```bash
redis-cli -h <redis-endpoint> -p 6379 INFO stats | grep -E "rejected_connections|total_net_input_bytes"
# Check if writes are being denied due to OOM
redis-cli -h <redis-endpoint> -p 6379 SET _steward_canary_probe 1
```
- If the Canary SET command returns `(error) OOM command not allowed`, Redis has hit `maxmemory`!

### Step 3: Inspect Replication Status and Standby Health
```bash
redis-cli -h <redis-endpoint> -p 6379 INFO replication
```
- `role`: Must be `master`.
- `connected_slaves`: Must be $\ge 1$.
- `slave0: offset=..., lag=...`: `lag` should be 0 or 1. If lag $> 5$, the replica is falling behind.

---

## 3. Actionable Mitigation Runbooks

### Scenario A: Redis Memory Saturation (> 85% or OOM Rejections)
**Cause:** Key cardinality has exceeded the provisioned memory footprint (often due to sudden explosion in distinct client IP addresses or long-duration sliding logs).
**Immediate Remediation:**
1. **Dynamic Memory Scaling (Emergency):**
   If the underlying VM/container host has spare memory capacity:
   ```bash
   # Temporarily increase maxmemory to provide immediate breathing room
   redis-cli -h <redis-endpoint> CONFIG SET maxmemory <new_higher_limit_bytes>
   ```
2. **Scale Up Instance Type:**
   Scale up the managed Redis cluster instance type (e.g. AWS ElastiCache instance class) with an online failover.
3. **Audit High-Cardinality Policies:**
   Review active rate-limit descriptors for unbounded wildcard keys (e.g. tracking per-session-cookie rather than per-IP/account).

### Scenario B: Unplanned Redis Primary Crash / Outage
**Cause:** Primary hardware failure, kernel OOM kill, or network partition.
**Automated Handling:**
1. Cloud Orchestrator (e.g. AWS ElastiCache or Redis Sentinel) detects missed heartbeats and promotes the standby replica to primary.
2. The endpoint VIP or DNS record is updated to target the new primary.
3. **Steward Automatic Reconnection:**
   - Steward's `ConnectionManager` detects broken TCP sockets and retries connection in the background.
   - When the promoted primary accepts connections, Steward re-establishes TCP channels automatically without requiring pod restarts.
   - If the script cache is empty on the new primary, Steward intercepts `NOSCRIPT` and transparently reloads its Lua scripts (`fixed_window.lua`, `token_bucket.lua`, `sliding_window.lua`).

**Operator Actions during Outage:**
- Verify Envoy ingress traffic behavior:
  - If `failure_mode_deny: false`: Ingress traffic is admitted (fail-open) while preserving overall upstream availability.
  - If `failure_mode_deny: true`: Ingress traffic is rejected (fail-closed) to protect protected backends.
- Monitor `redis.errors` and `redis.timeouts` in the Grafana dashboard to confirm the moment of reconnection.

### Scenario C: Standby Replica Disconnected (`connected_slaves == 0`)
**Cause:** Replica crashed, rebooted, or the replication buffer overflowed.
**Actions:**
1. Check replica pod/instance health and system logs (`dmesg`, container exit codes).
2. Check replication buffer settings:
   ```bash
   redis-cli -h <primary-host> CONFIG GET client-output-buffer-limit
   ```
3. Restart or reprovision the standby replica to restore high-availability redundancy.

---

## 4. Post-Incident Verification

1. `redis.errors` and `redis.timeouts` drop back to 0.
2. `redis_memory_used_bytes / redis_memory_max_bytes` is $< 75\%$.
3. `connected_slaves` is $\ge 1$.
4. Canary probe write succeeds without error:
   ```bash
   redis-cli -h <redis-endpoint> SET _steward_canary_probe ok EX 10
   ```
