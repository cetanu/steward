# Runbook: Emergency Traffic Bypass & Load Shedding Procedures

**Target Scenarios:**
- Catastrophic storage failure (Redis cluster destroyed or partition unrecoverable).
- Unexpected zero-day bug or widespread false-positive rate limiting blocking all critical ingress traffic.
- Business leadership decision prioritizing 100% ingress availability over rate-limit protection.

---

## 1. Overview & Strategy

When Steward or its storage backend suffers catastrophic failure, operators have four escalating tiers of emergency bypass options, ranging from zero-downtime policy overrides to total Envoy filter bypass.

```
       [ Tier 1: Zero-Downtime Permissive Policy Push ]
                           │
       [ Tier 2: Envoy Fail-Open Configuration (failure_mode_deny: false) ]
                           │
       [ Tier 3: Selective Route-Level Action Disablement ]
                           │
       [ Tier 4: Total Envoy RLS Filter Disablement ]
```

---

## 2. Emergency Bypass Options

### Tier 1: Zero-Downtime Permissive Policy Push (Fastest & Safest)
*No Envoy or Steward pod restarts required. Preserves logging and metrics.*

1. Update the active rate-limit configuration at its source:
   - **For File Source (`ConfigSource::File`):**
     Edit the rate-limit configuration file directly on the host or update the orchestration ConfigMap:
     ```bash
     # Direct file edit:
     nano /etc/steward/rate-limits.json

     # Or Kubernetes ConfigMap:
     kubectl edit configmap steward-rate-limits -n steward-system
     ```
   - **For HTTP Source (`ConfigSource::Http`):**
     Update the JSON payload served by your upstream configuration endpoint.

   Set `requests_per_unit` to a massive number (e.g. `2000000000` / 2 billion) on all active paths:
   ```json
   {
     "domain": "default",
     "descriptors": [
       {
         "key": "remote_address",
         "rate_limit": { "unit": "seconds", "requests_per_unit": 2000000000 }
       }
     ]
   }
   ```
2. Within one refresh interval (plus +/-20% jitter), all Steward replicas fetch and compile the updated permissive policy automatically.
3. Ingress traffic is 100% admitted without touching Envoy infrastructure or recycling pods.

---

### Tier 2: Envoy Fail-Open Toggling (`failure_mode_deny: false`)
*Use when Redis is completely down and Steward is returning `Unavailable`.*

1. In Envoy's configuration ([`containers/envoy.yaml`](file:///home/vsyrakis/Documents/steward/containers/envoy.yaml) or your production Helm/ConfigMap):
   Locate `envoy.filters.http.ratelimit`:
   ```yaml
   http_filters:
     - name: envoy.filters.http.ratelimit
       typed_config:
         "@type": type.googleapis.com/envoy.extensions.filters.http.ratelimit.v3.RateLimit
         domain: default
         failure_mode_deny: false  # Ensure this is set to false
         rate_limit_service:
           grpc_service:
             envoy_grpc:
               cluster_name: rls
             timeout: 0.020s
   ```
2. Apply the change to Envoy (via dynamic LDS/xDS or rolling pod restart).
3. **Outcome:** Envoy continues calling Steward. When Steward returns `Status::unavailable` (or Envoy's 20ms timeout expires), Envoy catches the error, increments `ratelimit.failure_mode_allowed`, and admits the request through to the upstream application without blocking traffic.

---

### Tier 3: Selective Route Rate-Limit Disable
*Use when only a specific API endpoint or critical tenant is impacted.*

In Envoy's `route_config`:
```yaml
routes:
  - name: critical_checkout_api
    match:
      prefix: /checkout
    route:
      cluster: payment_backend
    # Comment out or delete rate_limits block for this route:
    # rate_limits:
    #   - actions:
    #       - remote_address: {}
```
Reload Envoy config. Ingress requests to `/checkout` immediately bypass rate-limiting while remaining routes continue to be protected.

---

### Tier 4: Total Envoy RLS Filter Disablement (Nuclear Option)
*Use only if Envoy itself is unstable due to connection pool exhaustion.*

1. In Envoy's `http_connection_manager`:
   Comment out or remove the `envoy.filters.http.ratelimit` filter entry entirely.
2. Envoy will route directly to upstream clusters without executing any RLS gRPC calls.

---

## 3. Restoring Enforcement After Incident

Once the underlying issue (e.g. Redis hardware, network partition, or configuration bug) is resolved:

1. **Verify Steward Health Directly:**
   ```bash
   # Test gRPC health probe on all replicas
   grpc-health-probe -addr=steward-service:5001
   ```
2. **Step-Down Bypass Tiers:**
   - If Tier 4 was used: Re-enable `envoy.filters.http.ratelimit` in staging/canary Envoy pods first.
   - If Tier 2 was used: Confirm `failure_mode_deny: false` allows zero-error traffic before toggling back to `failure_mode_deny: true` (if fail-closed is your policy).
   - If Tier 1 was used: Restore normal rate limits gradually in configuration.
3. **Verify Decision Telemetry:**
   In Grafana, confirm that `rate_limit.allowed` and `rate_limit.denied` resume realistic non-zero ratios without elevating `rate_limit.error`.
