import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { evaluateCanary } from '../scripts/canary-gate.mjs';

const policy = {
  schemaVersion: 1,
  targets: {
    backend: {
      canaryPercent: 10,
      observationWindowSeconds: 300,
      success: {
        minHealthSuccessRate: 0.99,
        maxErrorRate: 0.01,
        maxP95LatencyMs: 2000,
      },
      autoStop: true,
    },
    frontend: {
      canaryPercent: 10,
      observationWindowSeconds: 300,
      success: {
        minHealthSuccessRate: 0.99,
        maxErrorRate: 0.02,
        maxP95LatencyMs: 3000,
      },
      autoStop: true,
    },
  },
  manualOverride: { allowed: true, requireReason: true, auditField: 'overrideReason' },
};

describe('canary gate (#112)', () => {
  it('progresses a healthy backend release', () => {
    const audit = evaluateCanary({
      policy,
      target: 'backend',
      metrics: {
        healthSuccessRate: 0.995,
        errorRate: 0.004,
        p95LatencyMs: 500,
        observedSeconds: 300,
      },
    });
    assert.equal(audit.decision, 'progress');
  });

  it('halts a synthetic failing deployment', () => {
    const audit = evaluateCanary({
      policy,
      target: 'backend',
      metrics: {
        healthSuccessRate: 0.5,
        errorRate: 0.4,
        p95LatencyMs: 9000,
        observedSeconds: 300,
      },
    });
    assert.equal(audit.decision, 'halt');
    assert.ok(audit.reasons.length > 0);
  });

  it('halts when observation window is incomplete', () => {
    const audit = evaluateCanary({
      policy,
      target: 'frontend',
      metrics: {
        healthSuccessRate: 1,
        errorRate: 0,
        p95LatencyMs: 100,
        observedSeconds: 30,
      },
    });
    assert.equal(audit.decision, 'halt');
  });

  it('manual override progresses with audit trail', () => {
    const audit = evaluateCanary({
      policy,
      target: 'frontend',
      metrics: {
        healthSuccessRate: 0.5,
        errorRate: 0.5,
        p95LatencyMs: 9999,
        observedSeconds: 300,
      },
      overrideReason: 'staging drill approved by on-call',
    });
    assert.equal(audit.decision, 'progress_override');
    assert.equal(audit.override.reason, 'staging drill approved by on-call');
  });

  it('fail closed on missing metrics', () => {
    const audit = evaluateCanary({
      policy,
      target: 'backend',
      metrics: null,
    });
    assert.equal(audit.decision, 'halt');
  });
});
