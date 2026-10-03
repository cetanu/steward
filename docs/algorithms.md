# Steward Rate Limiting Algorithms Specification

This document defines the normative mathematical models, state representations, guarantees, boundary behaviors, time sources, and limits for all rate-limiting algorithms implemented in Steward.

---

## 1. Overview & Architecture

Steward provides three rate-limiting algorithms, each optimized for distinct operational and traffic shaping characteristics:

| Dimension | Fixed Window (`fixed_window`) | Token Bucket (`token_bucket`) | Sliding Window (`sliding_window`) |
| :--- | :--- | :--- | :--- |
| **Redis Data Structure** | Redis String (integer counter) | Redis Hash (`tokens`, `timestamp_ms`) | Redis Sorted Set (ZSET) |
| **State Complexity** | $O(1)$ space | $O(1)$ space | $O(N)$ space ($N \le 10{,}000$) |
| **Time Complexity** | $O(1)$ per evaluation | $O(1)$ per evaluation | $O(\log N + M)$ per evaluation |
| **Time Source** | Redis TTL / Server epoch | Authoritative Redis `TIME` (ms) | Authoritative Redis `TIME` (ms + $\mu$s) |
| **Boundary Burst** | Up to $2 \times$ across boundaries | Bounded to burst capacity $C$ | $0 \times$ overshoot (exact trailing window) |
| **Refill Model** | Step-function (at window reset) | Continuous fractional ($C / W_{\text{ms}}$) | Event expiration at $t - W_{\text{ms}}$ |
| **Refunds Supported** | Yes (clamped to 0) | Yes (clamped to $C$) | No (explicitly rejected) |
| **Max Hit Cost** | Unbounded ($\le \text{limit}$) | Bounded by capacity $C$ | Hard cap: 100 hits per RPC |
| **Retention Bounds** | 1 key, TTL = window duration | 1 key, PTTL = $2 \times W_{\text{ms}}$ | 1 ZSET, capped at 10,000 members |

All algorithms execute atomically inside Redis via Lua scripts (`EVALSHA`), guaranteeing that individual key operations are serialized without race conditions or partial updates.

---

## 2. Fixed Window Algorithm (`fixed_window`)

### 2.1 Mathematical Model
The fixed window algorithm segments time into uniform, non-overlapping windows of duration $W$.

In Steward, fixed windows are evaluated via atomic increment:
1. Upon request with hit cost $H$:
   $$\text{current} \leftarrow \text{INCRBY}(K, H)$$
2. If $\text{current} = H$ (first hit in a new window lifecycle):
   $$\text{EXPIRE}(K, W)$$
3. Admission decision:
   $$\text{allowed} = \begin{cases} \text{true} & \text{if } \text{current} \le L \\ \text{false} & \text{if } \text{current} > L \end{cases}$$
   where $L$ is the configured limit (`requests_per_unit`).

### 2.2 State Representation
- **Key**: Canonical rate limit key string (e.g. `steward:{default}:v1:pol_1:path:fw:60s`).
- **Value**: Redis String containing a base-10 ASCII integer counter.
- **TTL**: Set via `EXPIRE` on first increment.

### 2.3 Refund Mechanics (`fixed_window_refund.lua`)
1. Reads existing value: $\text{current} \leftarrow \text{GET}(K)$.
2. If key does not exist, returns 0.
3. Computes: $\text{new\_val} = \max(0, \text{current} - H_{\text{refund}})$.
4. Writes: $\text{SET}(K, \text{new\_val}, \text{"KEEPTTL"})$.
5. Returns $\text{new\_val}$. Clamping at 0 prevents negative counters.

### 2.4 Guarantees and Boundary Behaviors
- **Memory & Compute:** Fixed $O(1)$ space and $O(1)$ time complexity. Lowest CPU overhead on Redis.
- **Boundary Burst (Double-Limit Anomaly):** If an identity sends $L$ requests at the very end of window $t$ (e.g., $t + W - \epsilon$) and another $L$ requests at the beginning of window $t+1$ (e.g., $t + W + \epsilon$), up to $2 \times L$ requests are admitted within an interval of $2\epsilon$.
- **Attempt Counting:** Denied requests increment the counter in Redis, recording demand even when throttled.

---

## 3. Token Bucket Algorithm (`token_bucket`)

### 3.1 Mathematical Model
The token bucket algorithm maintains a fractional token reserve refilled continuously over time up to a maximum burst capacity $C$, allowing traffic bursts while strictly enforcing a sustained average rate.

Given:
- Capacity $C$ (`requests_per_unit` tokens)
- Window duration $W_{\text{ms}}$ in milliseconds
- Continuous refill rate $r$:
  $$r = \frac{C}{W_{\text{ms}}} \quad \left(\frac{\text{tokens}}{\text{millisecond}}\right)$$

When an evaluation arrives at Redis server time $t_{\text{now}}$ with cost $H$:
1. **Clock Drift & Monotonicity Clamping:**
   Retrieve existing state $(\text{tokens}_{\text{prev}}, t_{\text{last}})$.
   If $t_{\text{last}}$ exists and $t_{\text{now}} < t_{\text{last}}$ (simulated or real backward clock shift):
   $$t_{\text{now}} \leftarrow t_{\text{last}}$$
2. **Token Initialization / Refill:**
   If key does not exist:
   $$\text{tokens} \leftarrow C, \quad t_{\text{last}} \leftarrow t_{\text{now}}$$
   Else:
   $$\Delta t = \max(0, t_{\text{now}} - t_{\text{last}})$$
   $$\text{tokens} \leftarrow \min(C, \text{tokens}_{\text{prev}} + \Delta t \times r)$$
3. **Admission & Consumption:**
   $$\text{allowed} = \begin{cases} 1 & \text{if } \text{tokens} \ge H \\ 0 & \text{if } \text{tokens} < H \end{cases}$$
   If $\text{allowed} = 1$:
   $$\text{tokens} \leftarrow \text{tokens} - H$$
4. **State Persistence & Expiration:**
   Write updated $(\text{tokens}, t_{\text{now}})$ to Redis Hash.
   Set expiry: $\text{PEXPIRE}(K, 2 \times W_{\text{ms}})$.
   Return $\{ \text{allowed}, \lfloor\text{tokens}\rfloor \}$.

### 3.2 State Representation
- **Key**: Canonical rate limit key string (e.g. `steward:{default}:v1:pol_api:path:tb:10s`).
- **Data Structure**: Redis Hash (`HSET`) with two fields:
  - `tokens`: IEEE 754 floating-point string representation of available tokens.
  - `timestamp_ms`: Integer string representation of the authoritative Redis millisecond timestamp.
- **TTL**: Refreshed to $2 \times W_{\text{ms}}$ on every evaluation to preserve idle token balances across small inactivity windows while preventing abandoned key leaks.

### 3.3 Authoritative Time Source & Backward Clock Drift Protection
- **Authoritative Server Time:** Scripts do not trust client application clocks. The Lua script invokes Redis `TIME` (`[seconds, microseconds]`), computing:
  $$t_{\text{now}} = \text{seconds} \times 1000 + \lfloor\text{microseconds} / 1000\rfloor$$
- **Clock Drift Clamping:** If NTP stepping or primary-replica failover causes $t_{\text{now}} < t_{\text{last}}$, the script clamps $t_{\text{now}} = t_{\text{last}}$. As a result:
  - Elapsed time $\Delta t = 0$.
  - No spurious token refill occurs.
  - The stored timestamp does not regress backwards.

### 3.4 Dynamic Capacity Updates on Live Keys
Because counter key identity depends on `domain`, `policy_id`, `path`, `algorithm`, and `unit` (but **not** on numeric capacity $C$), changing a policy's capacity in configuration retains the existing live key in Redis.
- When evaluated with a new capacity $C_{\text{new}}$:
  - Refill rate immediately becomes $r_{\text{new}} = C_{\text{new}} / W_{\text{ms}}$.
  - The accumulator caps tokens at $C_{\text{new}}$: $\text{tokens} = \min(C_{\text{new}}, \text{tokens} + \Delta t \times r_{\text{new}})$.
  - Existing token reserves are conserved up to the new ceiling without resetting state.

### 3.5 Refund Mechanics (`token_bucket_refund.lua`)
1. Refills tokens up to $t_{\text{now}}$ using standard elapsed math and clock clamping.
2. Credits refunded tokens: $\text{tokens} \leftarrow \min(C, \text{tokens} + H_{\text{refund}})$.
3. Persists state and refreshes PTTL. Returns $\{ 1, \lfloor\text{tokens}\rfloor \}$.

---

## 4. Sliding Window Algorithm (`sliding_window`)

### 4.1 Mathematical Model
The sliding window algorithm maintains an exact time-series event log of admitted requests in a trailing sliding window $[t_{\text{now}} - W_{\text{ms}}, t_{\text{now}}]$. It completely eliminates the $2 \times$ boundary burst of fixed windows.

Given:
- Limit $L$ (`requests_per_unit`)
- Window duration $W_{\text{ms}}$
- Request hit cost $H$

When a request arrives at Redis time $t_{\text{now}}$ (with microsecond component $u_{\text{now}}$):
1. **Input Validation:**
   $$1 \le H \le 100$$
   Requests with $H > 100$ or $H < 1$ are rejected immediately with $\{0, 0\}$.
2. **Exact Log Eviction:**
   $$\text{ZREMRANGEBYSCORE}(K, -\infty, t_{\text{now}} - W_{\text{ms}})$$
   Evicts all events older than the sliding window boundary.
3. **Capacity Assessment:**
   $$\text{current} \leftarrow \text{ZCARD}(K)$$
   $$\text{allowed} = \begin{cases} 1 & \text{if } \text{current} + H \le L \\ 0 & \text{if } \text{current} + H > L \end{cases}$$
4. **Log Insertion (Allowed Only):**
   If $\text{allowed} = 1$:
   For each $i \in \{1, \dots, H\}$, generate unique member identifier:
   $$\text{member}_i = u_{\text{now}} : \text{nonce} : i$$
   where $\text{nonce}$ is a 128-bit (32 hex character) cryptographically secure random value generated per request.
   $$\text{ZADD}(K, t_{\text{now}}, \text{member}_i)$$
   $$\text{current} \leftarrow \text{current} + H$$
5. **Retention Cap Enforcement (Bounded Work):**
   If $\text{current} > 10{,}000$:
   $$\text{ZREMRANGEBYRANK}(K, 0, \text{current} - 10{,}001)$$
   $$\text{current} \leftarrow 10{,}000$$
   Trims the oldest events to strictly bound Redis memory and CPU work.
6. **Persistence & Expiration:**
   $$\text{PEXPIRE}(K, 2 \times W_{\text{ms}})$$
   Return $\{ \text{allowed}, \text{current} \}$.

### 4.2 State Representation
- **Key**: Canonical rate limit key string (e.g. `steward:{default}:v1:pol_sw:path:sw:60s`).
- **Data Structure**: Redis Sorted Set (ZSET).
  - **Score**: Millisecond timestamp ($t_{\text{now}}$) from Redis `TIME`.
  - **Member**: `<usec>:<nonce>:<i>` where:
    - `<usec>`: Microsecond timestamp from Redis `TIME`.
    - `<nonce>`: 32-character hexadecimal string generated via CSPRNG (`ring::rand::SystemRandom`).
    - `<i>`: 1-based hit sequence index ($1 \le i \le H$).
- **TTL**: Refreshed to $2 \times W_{\text{ms}}$ on every evaluation.

### 4.3 Multi-Replica Concurrency & Collision Freedom
In distributed deployments with multiple Steward replicas serving identical rate-limit keys:
- Two replicas evaluating requests within the identical microsecond generate distinct 128-bit random nonces.
- Member collisions have probability $p < 2^{-128}$, effectively zero.
- Replicas never overwrite concurrent events in `ZADD`.
- Redis executes each Lua script atomically, ensuring total ordering of evictions, counts, and insertions.

### 4.4 Resource Bounds and Non-Refundable Guarantees
- **Hit Cost Bounding:** Hit cost is strictly bounded to $H \le 100$ per call to prevent unbounded Lua execution loops.
- **Log Retention Bounding:** Set size is strictly capped at $10{,}000$ members per key via `ZREMRANGEBYRANK`, preventing memory exhaustion.
- **Non-Refundable:** Sliding window log entries represent physical event timestamps. Arbitrary event refunds cannot delete historical timestamps safely without violating temporal ordering. Refunds on sliding window policies are explicitly rejected (`HitOperation::Refund` returns an error).
