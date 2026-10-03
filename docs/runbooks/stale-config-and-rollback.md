# Runbook: Stale Configuration, Loader Failures, and Emergency Rollback

**Target Alerts:**
- `StewardConfigurationStaleCritical`
- `StewardConfigurationReloadFailures`
- `StewardConfigurationFleetVersionDivergence`
- `StewardConfigurationLoaderCrash`

---

## 1. Overview & Architecture Context

Steward employs an asynchronous, non-blocking configuration loader ([`src/config_source.rs`](file:///home/vsyrakis/Documents/steward/src/config_source.rs)):
- **Whole-Snapshot Validation:** New policy files are validated in full (checking units, positive capacities, duplicate rules, and size bounds $\le 10\text{ MiB}$) before being compiled into an immutable `Arc<CompiledConfig>` trie.
- **Resilient Reloads:** If a reload fails (network error, syntax error, or semantic validation failure), Steward **retains the active snapshot untouched** without dropping traffic or restarting.
- **Maximum Stale Duration:** If the active snapshot age exceeds `max_stale_duration` (default: 3600 seconds / 1 hour), Steward raises a critical `config_stale = 1` alert.

---

## 2. Immediate Diagnostic Steps

### Step 1: Check Steward Logs for Exact Validation Errors
```bash
kubectl logs -l app=steward -n steward-system --tail=100 | grep -E "failed to reload|CRITICAL|validation"
```
Common log outputs:
- `invalid requests_per_unit (-5) for path '...'`: Policy author submitted a negative capacity.
- `duplicate rate limit rule for path '...'`: Redundant rule configured in the same domain.
- `config response Content-Length exceeds maximum allowed limit`: Response payload exceeded 10 MiB bound.
- `config endpoint returned an error: 500 Internal Server Error`: Upstream HTTP server failure.

### Step 2: Check Config Source Reachability
```bash
# Exec into a Steward pod to test upstream config connectivity
kubectl exec -it deployment/steward -n steward-system -- curl -sIv http://config-service:8000/steward.json
```

### Step 3: Check Fleet Version Consistency
In the **Steward SLO Dashboard**, inspect the **Fleet Configuration Version Convergence** panel:
- Are all pods reporting identical `config_version` values?
- If some pods report Version A and others report Version B, a partial rollout or localized configuration mount failure has occurred.

---

## 3. Actionable Remediation Runbooks

### Scenario A: Policy Syntax or Validation Error
**Cause:** The configuration source (file or HTTP endpoint) contains invalid policy content (e.g. invalid JSON/YAML, non-positive capacity, unknown unit, duplicate path rules, or payload > 10 MiB).
**Behavior:** Steward's background loader rejected the invalid content and safely kept the last valid configuration active without dropping traffic.
**Action:**
1. Check the exact validation error in Steward logs:
   ```bash
   kubectl logs deployment/steward -n steward-system --tail=50 | grep -E "failed to reload|validation"
   ```
2. Correct the offending configuration at its source:
   - **For File Source (`ConfigSource::File`):**
     Edit the configuration file or update the mounted Kubernetes ConfigMap/Secret:
     ```bash
     kubectl edit configmap steward-rate-limits -n steward-system
     ```
   - **For HTTP Source (`ConfigSource::Http`):**
     Fix the JSON payload served by the upstream HTTP endpoint so it returns valid policy schemas.
3. Observe Steward logs to confirm the next polling cycle succeeds:
   ```bash
   kubectl logs -f deployment/steward -n steward-system | grep "reloaded rate-limit configuration"
   ```

### Scenario B: Upstream HTTP Config Distribution Outage
**Cause:** The policy delivery service (e.g. internal control plane service, S3 bucket, or mock server) is unreachable, returning HTTP 5xx, or timing out.
**Actions:**
1. Check the health of the upstream HTTP configuration server and network connectivity.
2. If the HTTP endpoint cannot be recovered quickly, temporarily switch Steward to a fallback local file source via environment variable or ConfigMap:
   ```bash
   # Update STEWARD_CONFIG_PATH or rate_limit_configs source to a local file
   kubectl set env deployment/steward STEWARD__RATE_LIMIT_CONFIGS='{"file": "/etc/steward/fallback-limits.json"}' -n steward-system
   ```

### Scenario C: Fleet Version Divergence
**Cause:** A rolling update or volume mount propagation delay caused different pods to observe different configuration versions simultaneously.
**Actions:**
1. Check rollout status across pods:
   ```bash
   kubectl get pods -l app=steward -n steward-system -o wide
   ```
2. Verify that all pods mount the same ConfigMap volume or resolve the same HTTP configuration URL.

### Scenario D: Emergency Policy Rollback Procedure
If a newly published valid policy caused unexpected production rejections (e.g. a quota was set too strictly, impacting legitimate traffic):

1. **Rollback at the Configuration Source:**
   - **For File Source (`ConfigSource::File`):**
     Restore the previous known-good configuration file or rollback the Kubernetes ConfigMap to its prior revision:
     ```bash
     # Restore previous ConfigMap revision or backup file
     kubectl rollout undo deployment/steward -n steward-system
     # Or overwrite with known-good backup:
     cp /etc/steward/rate-limits.json.bak /etc/steward/rate-limits.json
     ```
   - **For HTTP Source (`ConfigSource::Http`):**
     Revert the policy payload served by the upstream HTTP endpoint to the previous known-good JSON snapshot (e.g. via your policy management API or object store versioning).
2. **Verify Fleet Sync:**
   Steward's background loader automatically detects the restored policy on its next jittered polling tick.
   Verify that `config.version` converges fleet-wide in the Grafana dashboard and `rate_limit.denied` returns to baseline.

---

## 4. Recovery Verification

1. `config_stale` gauge returns to `0`.
2. `config_consecutive_fetch_failures` resets to `0`.
3. `config_age_seconds` resets to `0`.
4. All Steward pods report identical `config_version` hash prefixes.
