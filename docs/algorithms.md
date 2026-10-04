# Rate Limiting Algorithms

Steward supports three rate-limiting algorithms, each tailored for different use cases and trade-offs.

## Algorithm Comparison

| Feature | Fixed Window (`fixed_window`) | Token Bucket (`token_bucket`) | Sliding Window (`sliding_window`) |
| :--- | :--- | :--- | :--- |
| **Redis Data Structure** | String (integer counter) | Hash (`tokens`, `timestamp_ms`) | Sorted Set (`ZSET`) |
| **Memory per Key** | ~128 bytes | ~210 bytes | ~120 bytes + ~50 bytes per event |
| **Time Complexity** | $O(1)$ | $O(1)$ | $O(\log N + M)$ |
| **Time Source** | Window expiration / TTL | Redis server time (`TIME`) | Redis server time (`TIME`) |
| **Boundary Spike** | Up to $2\times$ at window boundaries | Bounded to capacity | None (exact rolling window) |
| **Refill Behavior** | Step reset at window boundary | Continuous fractional refill | Event expiration after window duration |
| **Refunds Supported** | Yes | Yes | No |
| **Max Hit Cost** | Limited by configured quota | Limited by capacity | 100 hits per request |
| **Default** | Yes | No | No |

All operations are executed atomically in Redis using cached Lua scripts.

---

## 1. Fixed Window (`fixed_window`)

The fixed window algorithm is the default. It tracks request counts within fixed calendar intervals (e.g. 1 minute, 1 hour).

### How It Works
1. Each request increments an integer counter using Redis `INCRBY`.
2. On the first request in a window, a TTL matching the window duration is set using `EXPIRE`.
3. If the counter is less than or equal to `requests_per_unit`, the request is allowed. If greater, it is denied.

### Refunds
Refunds are supported using `fixed_window_refund.lua`. When a request with `is_negative_hits` arrives, the counter is decremented, clamped at 0 to prevent negative values.

### Trade-offs
- **Pros:** Lowest memory (~128 bytes/key) and lowest Redis CPU usage.
- **Cons:** Traffic spikes can double at window boundaries. For example, if a user sends their entire limit at the end of minute 1 and again at the start of minute 2, they can send $2\times$ their limit in a short span.

---

## 2. Token Bucket (`token_bucket`)

The token bucket algorithm provides smooth rate limiting with controlled burst capacity.

### How It Works
1. State is stored in a Redis Hash with two fields:
   - `tokens`: Current available tokens (floating point).
   - `timestamp_ms`: Millisecond timestamp of the last evaluation, obtained from Redis `TIME`.
2. On each request, Steward calculates the elapsed time since the last update and adds refilled tokens:
   $$\text{refill} = \Delta t \times \frac{\text{capacity}}{\text{window\_ms}}$$
   Tokens are capped at the maximum capacity (`requests_per_unit`).
3. If enough tokens are available for the hit cost, tokens are deducted and the request is allowed. Otherwise, it is denied.
4. Key expiration is set to $2\times$ the window duration to prevent abandoned keys from accumulating in memory.

### Clock Drift Protection
To protect against clock drift or NTP adjustments, Redis server time (`TIME`) is used rather than application client timestamps. If the current time appears earlier than `timestamp_ms`, time is clamped to avoid spurious token generation.

### Dynamic Capacity Updates
Counter key names depend on the domain, policy ID, path, and unit, but not the capacity. If you change a policy's capacity in configuration, the existing token balance is preserved up to the new limit without resetting the counter.

### Trade-offs
- **Pros:** Smooth traffic shaping, prevents boundary spikes, allows configuring controlled burst capacity.
- **Cons:** Slightly larger state per key (~210 bytes) than fixed window.

---

## 3. Sliding Window (`sliding_window`)

The sliding window algorithm maintains an exact rolling log of recent request timestamps. It completely eliminates window boundary spikes.

### How It Works
1. State is stored in a Redis Sorted Set (`ZSET`), where each member represents an event and the score is the Redis millisecond timestamp.
2. Expired events older than $(t_{\text{now}} - \text{window\_ms})$ are removed using `ZREMRANGEBYSCORE`.
3. The remaining events are counted with `ZCARD`. If $\text{current} + \text{hits} \le \text{limit}$, the request is allowed.
4. If allowed, new events are added to the set with unique members formatted as `<microsecond>:<nonce>:<index>`, where `<nonce>` is a random identifier. This prevents concurrent requests from different replicas from overwriting each other.
5. If the set grows larger than 10,000 events, the oldest entries are pruned using `ZREMRANGEBYRANK` to cap memory usage.

### Trade-offs
- **Pros:** Exact rolling enforcement with zero boundary overshoot.
- **Cons:** Higher memory and CPU usage on Redis proportional to the number of events. Refunds are not supported because historical event logs cannot be retroactively adjusted without breaking time ordering.
