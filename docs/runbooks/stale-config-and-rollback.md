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
# On Kubernetes:
kubectl logs -l app=steward -n steward-system --tail=100 | grep -E "failed to reload|CRITICAL|validation"

# On systemd / bare metal host:
journalctl -u steward -n 100 --no-pager | grep -E "failed to reload|CRITICAL|validation"

# In Docker container:
docker logs --tail 100 steward-server | grep -E "failed to reload|CRITICAL|validation"
```
Common log outputs:
- `invalid requests_per_unit (-5) for path '...'`: Policy author submitted a negative capacity.
- `duplicate rate limit rule for path '...'`: Redundant rule configured in the same domain.
- `config response Content-Length exceeds maximum allowed limit`: Response payload exceeded 10 MiB bound.
- `config endpoint returned an error: 500 Internal Server Error`: Upstream HTTP server failure.

### Step 2: Check Config Source Reachability
```bash
# Direct host or container test:
curl -sIv http://config-service:8000/steward.json

# Or inside a Kubernetes pod:
kubectl exec -it deployment/steward -n steward-system -- curl -sIv http://config-service:8000/steward.json
```

### Step 3: Check Fleet Version Consistency
In the **Steward SLO Dashboard**, inspect the **Fleet Configuration Version Convergence** panel:
- Are all replicas reporting identical `config_version` values?
- If some replicas report Version A and others report Version B, a partial rollout or localized configuration mount failure has occurred.

---

## 3. Actionable Remediation Runbooks

### Scenario A: Policy Syntax or Validation Error
**Cause:** The configuration source (file or HTTP endpoint) contains invalid policy content (e.g. invalid JSON/YAML, non-positive capacity, unknown unit, duplicate path rules, or payload > 10 MiB).
**Behavior:** Steward's background loader rejected the invalid content and safely kept the last valid configuration active without dropping traffic.
**Action:**
1. Check the exact validation error in Steward logs:
   ```bash
   # Kubernetes:
   kubectl logs deployment/steward -n steward-system --tail=50 | grep -E "failed to reload|validation"
   # Systemd:
   journalctl -u steward -n 50 --no-pager | grep -E "failed to reload|validation"
   ```
2. Correct the offending configuration at its source:
   - **For File Source (`ConfigSource::File`):**
     Edit the rate limit policy file directly or update the mounted ConfigMap/Secret:
     ```bash
     # Direct file:
     nano /etc/steward/rate-limits.json
     # Or Kubernetes ConfigMap:
     kubectl edit configmap steward-rate-limits -n steward-system
     ```
   - **For HTTP Source (`ConfigSource::Http`):**
     Fix the JSON payload served by the upstream HTTP endpoint so it returns valid policy schemas.
3. Observe Steward logs to confirm the next polling cycle succeeds:
   ```bash
   # Kubernetes:
   kubectl logs -f deployment/steward -n steward-system | grep "reloaded rate-limit configuration"
   # Systemd:
   journalctl -u steward -f | grep "reloaded rate-limit configuration"
   ```

### Scenario B: Upstream HTTP Config Distribution Outage
**Cause:** The policy delivery service (e.g. internal control plane service, S3 bucket, or mock server) is unreachable, returning HTTP 5xx, or timing out.
**Actions:**
1. Check the health of the upstream HTTP configuration server and network connectivity.
2. If the HTTP endpoint cannot be recovered quickly, temporarily switch Steward to a fallback local file source via environment variable:
   ```bash
   # Host / systemd environment override:
   systemctl edit steward  # Add: Environment="STEWARD__RATE_LIMIT_CONFIGS__FILE=/etc/steward/fallback-limits.json"
   systemctl restart steward

   # Kubernetes deployment:
   kubectl set env deployment/steward STEWARD__RATE_LIMIT_CONFIGS='{"file": "/etc/steward/fallback-limits.json"}' -n steward-system
   ```

### Scenario C: Fleet Version Divergence
**Cause:** A rolling update or volume mount propagation delay caused different replicas to observe different configuration versions simultaneously.
**Actions:**
1. Check replica status across nodes or pods:
   ```bash
   # Kubernetes:
   kubectl get pods -l app=steward -n steward-system -o wide
   # Systemd / Nomad / ECS:
   # Verify version status across all running instances via your orchestration dashboard.
   ```
2. Verify that all replicas mount the same configuration file volume or resolve the same HTTP configuration URL.

### Scenario D: Emergency Policy Rollback Procedure
If a newly published valid policy caused unexpected production rejections (e.g. a quota was set too strictly, impacting legitimate traffic):

1. **Rollback at the Configuration Source:**
   - **For File Source (`ConfigSource::File`):**
     Restore the previous known-good configuration file from backup or rollback the orchestration config:
     ```bash
     # Restore previous file from backup:
     cp /etc/steward/rate-limits.json.bak /etc/steward/rate-limits.json
     # Or Kubernetes ConfigMap undo:
     kubectl rollout undo deployment/steward -n steward-system
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
