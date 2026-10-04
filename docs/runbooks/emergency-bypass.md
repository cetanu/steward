# Runbook: Emergency Traffic Bypass

Use this runbook when an outage or misconfiguration requires temporarily bypassing rate limits to keep ingress traffic flowing.

---

## Bypass Options

Four options are available, ordered from least disruptive to most intrusive:

```
Option 1: Increase limit values in policy config (No Envoy/Steward changes)
    │
Option 2: Ensure Envoy fails open on errors (`failure_mode_deny: false`)
    │
Option 3: Remove rate-limiting from specific routes in Envoy
    │
Option 4: Remove the rate-limit filter from Envoy entirely
```

---

### Option 1: Increase Quota Limits in Policy Config
*Recommended first step. Does not require restarting Envoy or Steward, and preserves metrics/logging.*

1. In your rate-limit policy file or upstream configuration endpoint, set `requests_per_unit` to a very large number:
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
2. Steward reloads the configuration within its refresh interval (default: 60s) and starts admitting all requests.

---

### Option 2: Set Envoy to Fail-Open (`failure_mode_deny: false`)
*Use when Redis or Steward is completely down or unreachable.*

1. In Envoy's configuration (`envoy.yaml`), verify or set `failure_mode_deny: false`:
   ```yaml
   http_filters:
     - name: envoy.filters.http.ratelimit
       typed_config:
         "@type": type.googleapis.com/envoy.extensions.filters.http.ratelimit.v3.RateLimit
         domain: default
         failure_mode_deny: false
         rate_limit_service:
           grpc_service:
             envoy_grpc:
               cluster_name: rls
             timeout: 0.020s
   ```
2. When Steward returns an error or times out, Envoy allows the traffic through to your backend application and records the metric `ratelimit.failure_mode_allowed`.

---

### Option 3: Disable Rate-Limiting on Specific Routes
*Use when only a specific API endpoint or customer is affected.*

In Envoy's route configuration, remove or comment out the `rate_limits` block for the impacted route:

```yaml
routes:
  - name: checkout_route
    match:
      prefix: /checkout
    route:
      cluster: backend_service
    # Comment out rate_limits block:
    # rate_limits:
    #   - actions:
    #       - remote_address: {}
```

Reload or restart Envoy. Traffic to `/checkout` will bypass rate-limiting while other routes remain protected.

---

### Option 4: Remove the Rate-Limit Filter Entirely
*Use only if Envoy itself is experiencing issues communicating with the rate-limit cluster.*

1. In Envoy's `http_connection_manager` configuration, remove the `envoy.filters.http.ratelimit` filter entry.
2. Reload Envoy. All incoming requests will route directly to upstream services without contacting Steward.

---

## Restoring Rate Limiting After an Incident

Once the underlying issue (e.g. Redis connectivity or bad configuration) is resolved:

1. **Verify Steward Health:**
   ```bash
   grpc-health-probe -addr=steward-service:5001
   ```
2. **Reverse the Bypass Step:**
   - If Option 4 was used: Re-add the filter in Envoy staging or canary pods first.
   - If Option 3 was used: Re-enable the `rate_limits` block on the route.
   - If Option 1 was used: Restore normal quota limits in the configuration.
3. **Check Metrics:**
   Verify in Grafana or StatsD that `rate_limit.allowed` and `rate_limit.denied` metrics return to normal expected levels.
