# Runbook: Latency Degradation & Overload Shedding

Use this runbook when rate-limiting decision latency spikes or admission load shedding activates.

---

## 1. Context

Steward uses strict internal limits to protect service availability:
- **Internal Execution Deadline:** 10 ms (configurable via `execution_timeout_ms`).
- **Admission Concurrency Limit:** 1,024 concurrent requests per instance (configurable via `max_concurrent_requests`).

When concurrency reaches the limit, Steward sheds load immediately with gRPC `Status::ResourceExhausted` (`requests_rejected_admission`) to avoid queueing. If a request takes longer than 10 ms, Steward aborts it with `Status::DeadlineExceeded`.

---

## 2. Triage Flow

```
                      Alert Fired
                           │
           Is requests_rejected_admission > 0?
                 /                   \
               YES                    NO
               /                        \
    In-flight queue full             Check p99 latency
    (Load Shedding Active)                  │
               │                      Is Redis p99 high?
               │                           /        \
               │                         YES         NO
               ▼                          ▼           ▼
        Scale Steward Replicas      Check Redis   Check Steward
        or check Redis slowness     CPU & RTT     CPU usage
```

---

## 3. Diagnosis Steps

### Step 1: Check Redis vs Service Latency
Look at the Steward Grafana dashboard:
- **If Redis Latency $\approx$ Total Latency:** The delay is in Redis execution or network RTT.
- **If Total Latency $\gg$ Redis Latency:** The delay is in thread scheduling or CPU saturation in Steward.

### Step 2: Check Redis Slowlog and CPU
```bash
# Check CPU usage
redis-cli -h <redis-host> -p 6379 INFO cpu

# Check slow commands taking > 10ms
redis-cli -h <redis-host> -p 6379 SLOWLOG GET 25
```
Common causes:
- A sliding window descriptor processing very large event sets.
- `BGSAVE` or background AOF rewrites causing disk stalls.
- Redis CPU saturation on its single execution core.

### Step 3: Check Steward CPU and In-Flight Permits
```bash
# Kubernetes:
kubectl top pods -l app=steward

# Docker:
docker stats --no-stream steward-server
```

---

## 4. Remediation

### Scenario A: Admission Limit Exhaustion
**Cause:** More than 1,024 requests are in-flight concurrently, usually caused by a sudden traffic surge or slow Redis queries backing up requests.
**Fix:**
1. Scale up Steward replicas to distribute load:
   ```bash
   kubectl scale deployment steward --replicas=<count>
   ```
2. Verify that Envoy is load-balancing across all Steward instances evenly.

### Scenario B: Redis CPU Saturation
**Cause:** Single-threaded Redis core is saturated (CPU > 80%).
**Fix:**
1. Identify high-frequency keys in `SLOWLOG GET 25`.
2. If heavy sliding-window rules are responsible, consider switching them to token-bucket or fixed-window.
3. For large fleets, partition traffic across separate Redis instances by domain or tenant.

### Scenario C: High Network Latency (Cross-AZ Traffic)
**Cause:** Steward and Redis are located in different Availability Zones.
**Fix:**
Ensure Steward and Redis are deployed in the same AZ. Network RTT should be $\le 0.5\text{ ms}$.

---

## 5. Verification
- `requests_rejected_admission` returns to 0.
- `requests_deadline_exceeded` returns to 0.
- Decision latency p99 drops below 5 ms.
