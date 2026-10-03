# Runbook: Latency Degradation, Deadlines, and Admission Shedding

**Target Alerts:**
- `StewardHighDecisionLatencyP99`
- `StewardCriticalDecisionLatencyP99`
- `StewardRequestDeadlinesExceeded`
- `StewardAdmissionLoadSheddingActive`
- `StewardInFlightConcurrencyHigh`

---

## 1. Overview & Architecture Context

Steward operates on a strict latency budget ratified in [`docs/slo-and-limits.md`](../slo-and-limits.md):
- **Envoy RPC Timeout:** 20 ms.
- **Steward Internal Execution Deadline:** 10 ms.
- **Global Admission Semaphore:** 1,024 concurrent requests per instance.

When requests queue or Redis stalls, in-flight permits accumulate. If concurrency reaches 1,024, Steward sheds load immediately with gRPC `Status::resource_exhausted` (`requests_rejected_admission`) to prevent thread starvation and memory blowup. If individual request processing exceeds 10 ms, Steward aborts the request with `Status::deadline_exceeded`.

---

## 2. Rapid Triage Flowchart

```
                 Alert Fired
                      │
       Is requests_rejected_admission > 0?
             /                  \
          YES                    NO
          /                        \
In-flight permits exhausted     Check p99 latency
(Load Shedding Active)                 │
          │                     Is Redis p99 > 2ms?
          │                          /         \
          │                        YES          NO
          │                         │            │
          │                   Redis Slowlog   Steward CPU
          │                   or Network RTT  Saturation
          ▼                         ▼            ▼
   Scale Replicas /           Investigate     Scale HPA /
   Investigate Redis           Redis Core     Profile Locks
```

---

## 3. Immediate Diagnostic Steps

### Step 1: Identify Where the Time is Spent
Open the **Steward / Service Level Objectives & Operations** Grafana dashboard ([`steward-slo`](../monitoring/dashboards/steward-slo.json)):
1. Compare **Request Decision Latency** vs **Redis Backend Phase Latency**:
   - **If Redis Latency $\approx$ Total Latency:** The bottleneck is inside the Redis engine or the network RTT between Steward and Redis.
   - **If Total Latency $\gg$ Redis Latency:** The bottleneck is thread queuing, Tokio runtime exhaustion, or semaphore contention inside Steward.

### Step 2: Check Redis Slowlog and CPU
Connect to the Redis primary:
```bash
# Check instantaneous commands/sec and CPU
redis-cli -h <redis-host> -p 6379 INFO stats
redis-cli -h <redis-host> -p 6379 INFO cpu

# Inspect slow commands taking > 10ms
redis-cli -h <redis-host> -p 6379 SLOWLOG GET 25
```
Common Redis culprits:
- A sliding window descriptor accumulating heavy hit costs with large member sets.
- `BGSAVE` or AOF rewrite causing Copy-On-Write memory stalls on flash storage.
- CPU saturation (Redis is single-threaded for command execution; $> 80\%$ on the core causes rapid queueing).

### Step 3: Check Steward Fleet CPU and In-Flight Permits
```bash
# On Kubernetes:
kubectl top pods -l app=steward -n steward-system

# On systemd / bare metal host:
systemctl status steward
top -b -n 1 -p $(pgrep steward)

# In Docker container:
docker stats --no-stream steward-server
```
- If instances exceed 70% CPU, auto-scaling or instance provisioning may be lagging behind an unexpected offered-load spike.

---

## 4. Root Causes & Actionable Mitigations

### Scenario A: In-Flight Semaphore Exhaustion (Admission Load Shedding)
**Cause:** More than 1,024 requests are concurrently executing against a single Steward instance. This happens during high traffic spikes or when Redis slowdown backs up in-flight requests.
**Actions:**
1. **Scale Steward Replicas Immediately:**
   ```bash
   # Kubernetes:
   kubectl scale deployment steward --replicas=<current_replicas * 2> -n steward-system

   # Systemd / Bare Metal / Nomad / ECS:
   # Start additional systemd template instances (e.g. systemctl start steward@{2..4})
   # or increase task count in your supervisor/orchestrator (Nomad count, ECS desired count).
   ```
2. **Verify Envoy Load Balancing:**
   Check that traffic is distributing evenly across all backend instances rather than pinning to a single replica over persistent HTTP/2 connections.

### Scenario B: Redis CPU Saturation or Slow Commands
**Cause:** Heavy sliding-window evaluation or single-key hot-spotting consuming Redis single-threaded execution capacity.
**Actions:**
1. Identify offending keys in `SLOWLOG GET 25`.
2. If hot keys are identified:
   - Check if an upstream client is hammering a single endpoint.
   - Consider temporarily lowering `hits_addend` or applying an emergency override in policy.
3. If Redis primary CPU $> 85\%$:
   - Trigger horizontal sharding (Redis Cluster) or partition heavy domains onto dedicated Redis instances.

### Scenario C: Network Round-Trip Time Degradation
**Cause:** Cross-AZ traffic routing.
**Requirement:** Steward and Redis primary must reside in the **same Availability Zone** ($\text{RTT} \le 0.5\text{ ms}$).
**Actions:**
1. Run ping or TCP latency checks between Steward pod and Redis endpoint:
   ```bash
   # From a Steward container
   redis-cli -h <redis-host> --latency-history
   ```
2. If latency exceeds $1.0\text{ ms}$, verify subnet routing and ensure Steward pods are scheduled with AZ node affinity matching the Redis primary.

---

## 5. Recovery Verification

1. `requests_rejected_admission` drops to 0.
2. `requests_deadline_exceeded` drops to 0.
3. Decision latency $p99 \le 5.0\text{ ms}$ and $p99.9 \le 10.0\text{ ms}$.
4. Error budget burn rate recovers to $< 1.0$.
