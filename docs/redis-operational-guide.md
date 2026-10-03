# Steward Redis Operational Guide

**Topology, Sizing, Durability, and Failover Runbooks**

This document establishes the official operational architecture, capacity planning formulas, memory and eviction policies, and disaster recovery runbooks for Redis storage backends backing Steward instances. It resolves Finding **F16** from [`docs/production-readiness.md`](file:///home/vsyrakis/Documents/steward/docs/production-readiness.md) and completes Milestone **M3.5**.

---

## 1. Supported Topology & High Availability Architecture

Steward relies on Redis as an external authoritative state store for rate-limiting counters. Because rate-limiting RPCs execute on the synchronous critical path of incoming ingress traffic, the storage topology must balance extreme throughput, sub-millisecond round-trip times, and resilience against hardware and network failure.

```
                    ┌─────────────────────────┐
                    │      Envoy Proxy        │
                    └────────────┬────────────┘
                                 │ gRPC RLS
                    ┌────────────▼────────────┐
                    │    Steward Fleet        │
                    │  (Stateless Replicas)   │
                    └────────────┬────────────┘
                                 │ Async ConnectionManager
                    ┌────────────▼────────────┐
                    │  Redis Service / VIP    │
                    └──────┬───────────▲──────┘
             Writes / Reads│           │ Automatic
                           │           │ Failover
              ┌────────────▼─────────┐ │
              │ Redis Primary (AZ-a) ├─┘
              │  maxmemory-policy:   │
              │     noeviction       │
              └──────────┬───────────┘
                         │ Asynchronous
                         │ Replication
              ┌──────────▼───────────┐
              │ Redis Replica (AZ-a) │
              │  (Warm Standby)      │
              └──────────────────────┘
```

### 1.1 Supported Topologies

1. **Dedicated Managed Primary with Same-AZ Replica (Recommended):**
   - **Primary:** Dedicated single-node Redis instance (e.g. AWS ElastiCache, GCP Cloud Memorystore, or Redis Sentinel / Valkey).
   - **Replica:** At least one warm standby replica located in the **same Availability Zone** as the Steward fleet to guarantee network round-trip time $\text{RTT} \le 0.5\text{ ms}$.
   - **Multi-AZ Option:** A secondary read-replica may reside in an alternate AZ for disaster recovery, but active traffic must route exclusively to the same-AZ primary to prevent tail-latency degradation.

2. **Clustered Redis (Large-Scale Fleet):**
   - When policy key cardinality or write volume exceeds a single Redis primary's CPU capacity (typically > 60,000 operations/sec on modern hardware), Redis Cluster with hash-tag routing or multiple independent Redis instances partitioned by tenant/domain may be deployed.

### 1.2 Networking & Latency Budget

- **Target Latency:** Network $\text{RTT} \le 0.5\text{ ms}$ between Steward instances and Redis primary.
- **Client Management:** Steward utilizes an asynchronous `redis::aio::ConnectionManager` multiplexing traffic over persistent non-blocking TCP connections. PING checkouts are eliminated.
- **Failover Endpoint:** Steward must be configured with a stable virtual IP (VIP) or DNS hostname with a low TTL (e.g. 5–15 seconds) managed by the cloud provider or Sentinel.

---

## 2. Capacity Sizing & Memory Formulas

Redis stores rate-limit counters in memory. Because keys are dynamic and generated based on incoming descriptor combinations (e.g. `domain|policy|path|unit|window`), memory utilization is a function of active key cardinality and algorithm choice.

### 2.1 State Representations and Memory Footprints

| Algorithm | Redis Data Structure | Fields & State Stored | Average Memory per Key | Max Memory per Key |
| :--- | :--- | :--- | :--- | :--- |
| **Fixed Window** | String (`SET`/`INCRBY`) | 64-bit integer hit counter, TTL | ~128 bytes | ~150 bytes |
| **Token Bucket** | Hash (`HSET`) | `tokens` (i64), `timestamp_ms` (i64), TTL | ~210 bytes | ~250 bytes |
| **Sliding Window** | Sorted Set (`ZSET`) | Member: `<usec>:<nonce>:<idx>`, Score: millisecond timestamp, TTL | ~120 bytes + ~50 bytes / event | ~500 KiB (capped at 10,000 events) |

### 2.2 Sizing Formula

Total Redis memory required ($M_{\text{total}}$) consists of raw key data, memory allocator fragmentation headroom, replication backlog buffers, and client output buffers:

$$M_{\text{raw}} = \sum_{a \in \text{algorithms}} \left( N_a \times S_a \right)$$

Where:
- $N_a$ = Number of active distinct identities/keys within the maximum window duration (e.g., active users/IPs in the last 60 seconds).
- $S_a$ = Average key size for algorithm $a$ (from table above).

#### Memory Headroom & Buffer Multipliers:

$$M_{\text{headroom}} = M_{\text{raw}} \times F_{\text{jemalloc}}$$

- **Allocator Overhead ($F_{\text{jemalloc}}$):** Budget a **$1.4\times$ multiplier** (40% headroom) for jemalloc memory fragmentation and internal metadata overhead.
- **Replication Backlog Buffer ($M_{\text{repl}}$):**
  $$M_{\text{repl}} = \text{Write Rate (bytes/sec)} \times \text{Target Reconnection Window (sec)}$$
  *Example:* At 25,000 writes/sec (~2.5 MB/sec write stream), a 60-second disconnection window requires $2.5 \times 60 \approx 150\text{ MB}$.
- **Client Output Buffers ($M_{\text{client}}$):** Budget $64\text{ MB}$ for in-flight command buffers across the Steward replica fleet.

#### Total Provisioned Memory Calculation:

$$M_{\text{total}} = \left( M_{\text{raw}} \times 1.4 \right) + M_{\text{repl}} + M_{\text{client}}$$

$$\text{Redis Instance RAM} \ge \frac{M_{\text{total}}}{0.75}$$

*(Keep `maxmemory` set to 75% of instance RAM, leaving 25% for OS operations, background save fork/COW overhead, and networking buffers).*

### 2.3 Sizing Example

- **Workload:** 100,000 active client IP addresses enforcing:
  - 1 Fixed Window rule (per-second limit).
  - 1 Token Bucket rule (per-minute limit).
- **Calculations:**
  - $N_{\text{fixed}} = 100,000 \implies 100,000 \times 128\text{ B} = 12.8\text{ MB}$.
  - $N_{\text{token}} = 100,000 \implies 100,000 \times 210\text{ B} = 21.0\text{ MB}$.
  - $M_{\text{raw}} = 12.8 + 21.0 = 33.8\text{ MB}$.
  - $M_{\text{headroom}} = 33.8\text{ MB} \times 1.4 = 47.3\text{ MB}$.
  - $M_{\text{repl}} + M_{\text{client}} \approx 200\text{ MB}$.
  - $M_{\text{total}} \approx 250\text{ MB}$.
  - **Recommended Instance:** A 2 vCPU / 2 GiB or 4 GiB dedicated Redis primary provides massive capacity headroom for this workload.

---

## 3. Eviction Policy: Mandatory `noeviction`

### 3.1 Strict Mandate

Production Redis instances backing Steward **MUST** be configured with:

```text
maxmemory-policy noeviction
```

### 3.2 Rationale & Failure Mode Integration

1. **Eviction Destroys Rate-Limiting Guarantees:**
   If Redis is configured with an eviction policy like `volatile-lru` or `allkeys-lru`, memory exhaustion causes Redis to silently drop keys before their TTL expires.
   - For Fixed Window and Token Bucket, deleting an active counter key resets consumed quota to 0.
   - An abusive client or traffic spike exhausting Redis memory would cause their own rate-limit key to be evicted, granting the abuser an **unbounded rate-limit bypass**!

2. **Integration with F06 Error Precedence:**
   Under `maxmemory-policy noeviction`, when `maxmemory` is reached, write commands (`INCRBY`, `HSET`, `ZADD`) fail immediately with:
   `OOM command not allowed when used memory > 'maxmemory'`.
   - Steward catches this Redis error.
   - Under the ratified **F06 Error Precedence Rule** ([`docs/failure-policy.md`](file:///home/vsyrakis/Documents/steward/docs/failure-policy.md)), Steward logs the failure, increments `redis.errors`, and returns gRPC `Status::unavailable("rate limit storage backend is unavailable")`.
   - Envoy then applies its configured `failure_mode_deny` policy (`true` to fail-closed, or `false` to fail-open).
   - This ensures operators retain complete, deterministic control over failure behavior instead of suffering silent quota resets.

---

## 4. State Durability and Failover Behavior

### 4.1 Persistence Configuration

Rate-limiting state consists of short-lived operational counters (lifespans typically between 1 second and 1 day).

| Persistence Mode | Tradeoffs | Production Recommendation |
| :--- | :--- | :--- |
| **No Persistence (`save ""` / `appendonly no`)** | Maximum throughput, zero disk I/O stalls, lowest latency variance. | **Recommended for multi-replica managed setups** where replicas provide state redundancy. |
| **AOF (`appendfsync everysec`)** | At most 1 second of state lost on primary power loss. Adds disk I/O overhead. | Acceptable if compliance mandates disk persistence. |
| **RDB Snapshots (`save 900 1`)** | Fork/Copy-on-Write memory spikes during background save (`bgsave`). | **Avoid** during heavy write loads to prevent tail latency spikes. |

### 4.2 Replication Lag & Quota State Loss

1. **Replication Model:**
   Redis replicates mutations to standby replicas asynchronously. During an unexpected primary crash, mutations accepted by the primary but not yet acknowledged by the replica are lost during failover.

2. **State Loss Bounds:**
   - Standard replication lag in same-AZ deployments is typically **$< 50\text{ ms}$**.
   - Maximum expected state loss is bounded by the replication lag window.

3. **Natural Window Self-Healing:**
   - Rate limit keys carry short TTLs aligned with the configured window (e.g. 1 second or 1 minute).
   - Lost counter increments result only in up to one window interval of extra quota for affected keys.
   - Within $\le 60\text{ seconds}$, all state naturally rolls over and self-heals, eliminating long-term data divergence.

### 4.3 Script Cache NOSCRIPT Recovery

When a replica is promoted to primary or when Redis is restarted:
- The Lua script cache (`SCRIPT LOAD`) on the new primary may be empty.
- Steward's Redis layer automatically detects `NOSCRIPT` errors, executes `SCRIPT LOAD` asynchronously to register the required Lua scripts, and transparently retries the rate-limit evaluation without dropping requests.

---

## 5. Failover Qualification & Operational Runbooks

### 5.1 Verification Checklist

Before qualifying a Redis deployment for production:
- [ ] Redis primary and replica are in the same AZ as Steward.
- [ ] Network RTT between Steward and Redis $\le 0.5\text{ ms}$.
- [ ] `maxmemory-policy noeviction` is explicitly verified via `redis-cli CONFIG GET maxmemory-policy`.
- [ ] Sizing calculation validates sufficient memory headroom for peak active identities.
- [ ] Automatic failover promotes replica within 5–15 seconds.
- [ ] Steward recovers connection automatically without restarting.

### 5.2 Planned Maintenance Failover Runbook

When performing planned Redis maintenance (e.g. engine upgrades or instance type scaling):

1. **Verify Replica Health:**
   ```bash
   redis-cli -h <primary-host> INFO replication
   # Confirm connected_slaves >= 1 and lag is 0
   ```
2. **Execute Controlled Failover:**
   - In AWS ElastiCache: Trigger "Reboot with Failover".
   - In Redis Sentinel: Execute `SENTINEL FAILOVER <master-name>`.
3. **Observe Steward Telemetry:**
   - Monitor `redis.operation_time_p99`: Brief expected bump during DNS/VIP flip.
   - Monitor `redis.errors`: Transient connection errors during the 1–3 second cutover.
   - Confirm Steward automatically re-establishes connection and resumes normal decision processing without service restart.

### 5.3 Unplanned Outage & Node Recovery Runbook

If the Redis primary crashes or terminates unexpectedly:

1. **Envoy Behavior:**
   During the failover transition, requests that cannot reach Redis receive gRPC `Unavailable` status. Envoy handles this according to `failure_mode_deny`:
   - `failure_mode_deny: false`: Ingress traffic is admitted (fail-open) while preserving backend availability.
   - `failure_mode_deny: true`: Ingress traffic is rejected (fail-closed) to protect downstream services from overload.
2. **Failover Execution:**
   The orchestrator/Sentinel promotes the standby replica to primary and updates the virtual IP or DNS record.
3. **Steward Automatic Recovery:**
   Steward's `ConnectionManager` detects broken TCP sockets, initiates exponential backoff reconnects, and restores full service as soon as the promoted primary accepts connections.
4. **Post-Incident Inspection:**
   ```bash
   # Check memory usage and OOM rejections
   redis-cli -h <new-primary> INFO memory
   # Check client connections
   redis-cli -h <new-primary> INFO clients
   ```

---

## 6. Recommended Alerting Thresholds

| Metric | Warning Threshold | Critical Threshold | Action |
| :--- | :--- | :--- | :--- |
| **Redis Used Memory %** | $\ge 75\%$ of `maxmemory` | $\ge 85\%$ of `maxmemory` | Scale up instance memory or review policy key cardinality. |
| **Replication Lag** | $\ge 100\text{ ms}$ | $\ge 1\text{ s}$ | Investigate network congestion or primary write saturation. |
| **Redis CPU Utilization** | $\ge 65\%$ on primary | $\ge 80\%$ on primary | Plan Redis sharding/clustering or review high-cardinality sliding-log rules. |
| **Steward `redis.errors`** | $> 0.01\%$ of QPS | $> 0.1\%$ of QPS | Investigate Redis connectivity, OOM rejections, or failover state. |
