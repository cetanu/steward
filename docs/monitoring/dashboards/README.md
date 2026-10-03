# Steward Grafana Dashboards

This directory contains production monitoring and Service Level Objective (SLO) dashboard definitions for the Steward rate-limit service.

## Available Dashboards

- [`steward-slo.json`](steward-slo.json): Comprehensive Service Level Objective (SLO), Traffic Breakdown, Decision Latency, Admission Control, and Configuration Freshness dashboard.

---

## 1. Dashboard Structure & Panels

The `steward-slo` dashboard is organized into logical operational rows:

### Row 1: Service Level Objectives (SLOs)
- **Enforcement Availability SLO Gauge:** Visualizes compliance against the ratified $\ge 99.99\%$ rolling 30-day enforcement availability contract.
- **1-Hour Error Budget Burn Rate:** Tracks whether transient errors or timeouts are rapidly depleting the 0.01% error budget (Alerts at $14.4\times$ 1-hour burn rate).
- **Active Policy Age:** Real-time gauge of the oldest configuration snapshot currently serving traffic across the replica fleet.
- **Stale Snapshot Status:** Binary indicator (`0` = FRESH, `1` = STALE CRITICAL) triggering if snapshot age exceeds `max_stale_duration` (3600 seconds).

### Row 2: Traffic Decisions & Admission Load Shedding
- **Decision Throughput by Outcome (QPS):** Stacked real-time visualization of:
  - `Allowed (OK)`: Admitted traffic within quota.
  - `Denied (OverLimit)`: Requests rejected due to quota exhaustion.
  - `Errors (Unavailable)`: Storage backend failure (propagated to Envoy per F06).
  - `Shed (ResourceExhausted)`: Load shedding rejections when in-flight concurrency reaches 1,024 permits.
- **Admission Control & In-Flight Concurrency:** Monitored against the 1,024 permit global ceiling. Tracks `requests.in_flight`, `requests.rejected_admission`, and `requests.deadline_exceeded`.

### Row 3: Latency Percentiles (SLO Targets)
- **Request Decision Latency:** $p50$, $p99$ allowed, $p99$ denied, and $p99.9$ overall latency percentiles. Evaluated against the $p99 \le 5\text{ ms}$ and $p99.9 \le 10\text{ ms}$ SLO budgets.
- **Redis Backend Phase Latency:** Dedicated $p50$, $p95$, and $p99$ timers measuring the raw Redis command execution round-trip time.

### Row 4: Redis Storage Backend & Script Health
- **Redis Errors & Timeouts:** Counts of connection errors, socket timeouts, and transparent `NOSCRIPT` script cache reload events.
- **Redis Primary Memory & Replication Health:** Memory saturation percentage against `maxmemory` (Warning: $\ge 75\%$, Critical: $\ge 85\%$) and connected replica count.

### Row 5: Configuration Lifecycle & Version Convergence
- **Fleet Configuration Version Convergence:** Displays the hex version digest prefix for all running replicas to detect version divergence during rolling deployments or localized loader failures.
- **Configuration Loader Health:** Monitors consecutive fetch failure counts, successful reload rates, and HTTP conditional `304 Not Modified` verifications.

---

## 2. Importing into Grafana

1. Open Grafana UI -> **Dashboards** -> **New** -> **Import**.
2. Upload `docs/monitoring/dashboards/steward-slo.json` or paste the JSON content.
3. Select your Prometheus / VictoriaMetrics data source variable (`DS_PROMETHEUS`).
4. Click **Import**.
