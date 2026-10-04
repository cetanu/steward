# Redis Operational Guide

This guide covers recommended Redis topology, capacity planning, eviction settings, and failover behavior for Steward.

---

## 1. Architecture & Network Topology

Steward uses Redis as its shared counter store. Because rate-limit checks run synchronously on incoming requests, the connection between Steward and Redis must be fast and reliable.

```
[ Envoy Proxy ] ──gRPC──> [ Steward Cluster ] ──TCP──> [ Redis Primary ]
                                                             │
                                                    async replication
                                                             │
                                                             ▼
                                                    [ Redis Replica (Standby) ]
```

### Topology Recommendations
- **Primary Node:** Dedicated single-node Redis instance (AWS ElastiCache, GCP Memorystore, or self-hosted Redis Sentinel).
- **Replica Node:** At least one standby replica in the **same Availability Zone** as the Steward fleet to maintain network round-trip time $\text{RTT} \le 0.5\text{ ms}$.
- **Multi-AZ Replicas:** Secondary replicas in another AZ can be kept for disaster recovery, but active traffic should target the same-AZ primary to prevent tail latency.
- **Failover Endpoint:** Point Steward to a stable virtual IP or DNS name with a short TTL (5–15 seconds).

---

## 2. Memory Sizing & Capacity Planning

Redis memory usage depends on the number of active rate-limit keys and the algorithm used:

| Algorithm | Stored State | Approximate Memory per Key |
| :--- | :--- | :--- |
| **Fixed Window** | String counter + TTL | ~128 bytes |
| **Token Bucket** | Hash (`tokens`, `timestamp_ms`) + TTL | ~210 bytes |
| **Sliding Window** | Sorted Set of event timestamps | ~120 bytes + ~50 bytes per event (capped at 10,000) |

### Sizing Rule of Thumb
To calculate required Redis memory:
1. Estimate the number of active identities $N$ across all policies within a window (e.g. 100,000 active client IPs).
2. Calculate raw memory:
   $$\text{Raw Memory} = N \times \text{Average Key Size}$$
3. Add a $1.5\times$ multiplier for allocator overhead, replication buffers, and client connections.
4. Set Redis `maxmemory` to roughly **75% of total instance RAM**, reserving 25% for OS operations, background saving, and networking buffers.

**Example:**
For 100,000 active clients with 1 fixed window rule (~13 MB) and 1 token bucket rule (~21 MB):
- Raw key memory: ~34 MB
- With headroom and buffers: ~100–150 MB
- Recommended instance: A standard 2 vCPU / 2–4 GiB Redis instance provides ample headroom.

---

## 3. Eviction Policy: `noeviction`

Production Redis instances backing Steward **must** be configured with:

```text
maxmemory-policy noeviction
```

### Why Eviction Must Be Disabled
If Redis uses an eviction policy like `allkeys-lru` or `volatile-lru`, memory pressure will silently evict active counter keys before their TTL expires.
- Evicting a counter resets the user's consumed quota to 0.
- An abusive client sending a massive traffic flood could trigger key eviction and grant themselves an **unbounded rate-limit bypass**.

With `noeviction`, Redis explicitly rejects writes with an out-of-memory (OOM) error if memory fills up. Steward catches this error and returns a gRPC `Unavailable` status, allowing Envoy to apply its configured failure policy (fail-open or fail-closed) predictably.

---

## 4. Durability & Persistence

Rate-limit counters are transient operational data with lifetimes typically ranging from seconds to hours.

| Configuration | Setting | Trade-off | Recommended For |
| :--- | :--- | :--- | :--- |
| **No Persistence** | `save ""` / `appendonly no` | Maximum throughput, zero disk I/O, lowest tail latency | Recommended for multi-replica setups |
| **AOF** | `appendfsync everysec` | Loses at most 1 second of data on crash, slight disk overhead | Compliance environments |
| **RDB Snapshots** | `save 900 1` | Background forks (`bgsave`) can cause memory spikes and latency jitter | Not recommended during heavy write loads |

### Replication and Failover Data Loss
Redis replication to standbys is asynchronous. During an unexpected primary crash, writes from the last few milliseconds of traffic may not have reached the replica. Because rate-limit keys have short window durations (e.g. 1s or 60s), any lost state naturally clears within one window duration.

### Script Cache (`NOSCRIPT`) Recovery
When a new primary is promoted, its Lua script cache may be empty. Steward automatically catches `NOSCRIPT` errors, reloads the script in the background, and retries the evaluation transparently.

---

## 5. Operational Commands & Maintenance

### Checking Memory & OOM Status
```bash
redis-cli -h <redis-host> -p 6379 INFO memory
redis-cli -h <redis-host> -p 6379 INFO stats | grep -E "rejected_connections|total_net_input_bytes"
```

### Checking Replication Health
```bash
redis-cli -h <redis-host> -p 6379 INFO replication
# Verify role is master and connected_slaves >= 1
```

### Performing a Controlled Failover
When upgrading Redis or changing instance sizes:
- **AWS ElastiCache:** Use "Reboot with Failover" in the console or CLI.
- **Redis Sentinel:** Run `SENTINEL FAILOVER <master-name>`.

Steward's connection manager automatically detects broken sockets and reconnects to the new primary within seconds.
