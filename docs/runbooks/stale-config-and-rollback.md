# Runbook: Stale Configuration & Rollback

Use this runbook when configuration reloads fail, configuration becomes stale, or a policy needs to be rolled back.

---

## 1. Context

Steward periodically polls its configuration source (local file or HTTP endpoint):
- **Validation:** New policies are validated in full before activation (checking for non-positive capacities, invalid units, duplicate paths, or payloads > 10 MiB).
- **Failure Handling:** If a reload fails, Steward keeps the current valid configuration active without interruption.
- **Stale Alert:** If configuration cannot be reloaded for longer than `max_stale_duration_secs` (default: 1 hour), a `config_stale = 1` alert fires.

---

## 2. Diagnosis

### Check Steward Logs for Reload Errors
```bash
# Kubernetes:
kubectl logs -l app=steward --tail=50 | grep -E "failed to reload|validation|config"

# Docker:
docker logs --tail 50 steward-server | grep -E "failed to reload|validation|config"
```

Common error causes:
- Invalid JSON or YAML syntax.
- Non-positive capacity (e.g. `requests_per_unit: 0` or negative).
- Unknown rate-limit unit.
- Payload exceeds 10 MiB maximum size.
- Upstream HTTP configuration server returned 5xx or timed out.

### Test HTTP Config Endpoint (if using HTTP source)
```bash
curl -sIv http://<config-service>/rate_limits.json
```

---

## 3. Remediation

### Scenario A: Invalid Policy Syntax or Values
1. Fix the error in the configuration file or the payload served by the HTTP endpoint.
2. Steward will automatically pick up the valid configuration on its next poll interval (default: 60s).

### Scenario B: HTTP Config Server Outage
1. Restore the upstream configuration service.
2. If the HTTP endpoint will be down for an extended period, temporarily switch Steward to a local fallback file via environment variable:
   ```bash
   STEWARD__RATE_LIMIT_CONFIGS__FILE=/etc/steward/fallback-limits.json
   ```

### Scenario C: Emergency Policy Rollback
If a newly deployed policy is too restrictive and blocking legitimate traffic:
1. Revert the file or HTTP endpoint payload to the previous known-good version.
2. Within the next polling interval, all Steward instances compile the reverted policy and resume normal enforcement.

---

## 4. Verification
1. `config_stale` metric returns to `0`.
2. `config_consecutive_fetch_failures` resets to `0`.
3. Steward logs confirm: `reloaded rate-limit configuration (version: <hash>)`.
