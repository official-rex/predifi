# Canary release gates

Policy: `deploy/canary/policy.json`  
Evaluator: `scripts/canary-gate.mjs`  
Workflow: `.github/workflows/canary-release.yml`

## Policy knobs

| Knob | Meaning |
|------|---------|
| `canaryPercent` | Share of traffic on the new revision |
| `observationWindowSeconds` | Minimum observe time before promote |
| `success.*` | Health, error rate, latency thresholds |
| `autoStop` | Halt promotion when thresholds fail |
| `manualOverride` | Requires reason; written to audit JSON |

## Staging validation

**Healthy (should progress):**

```json
{"healthSuccessRate":0.995,"errorRate":0.004,"p95LatencyMs":500,"observedSeconds":300}
