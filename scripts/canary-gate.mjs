#!/usr/bin/env node
/**
 * Canary gate evaluator for backend/frontend releases (#112).
 * Fail closed: bad/missing metrics → halt (unless explicit manual override).
 *
 * Usage:
 *   node scripts/canary-gate.mjs --target backend --metrics path/to/metrics.json
 *   node scripts/canary-gate.mjs --target frontend --metrics metrics.json --override "reason"
 *
 * Metrics JSON shape:
 * {
 *   "healthSuccessRate": 0.995,
 *   "errorRate": 0.004,
 *   "p95LatencyMs": 800,
 *   "observedSeconds": 300
 * }
 */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(__dirname, '..');

export function loadPolicy(policyPath = path.join(ROOT, 'deploy/canary/policy.json')) {
  return JSON.parse(fs.readFileSync(policyPath, 'utf8'));
}

export function evaluateCanary({ policy, target, metrics, overrideReason }) {
  const audit = {
    at: new Date().toISOString(),
    target,
    decision: null,
    reasons: [],
    override: null,
  };

  const cfg = policy?.targets?.[target];
  if (!cfg) {
    audit.decision = 'halt';
    audit.reasons.push(`unknown target: ${target}`);
    return audit;
  }

  if (metrics == null || typeof metrics !== 'object') {
    audit.decision = 'halt';
    audit.reasons.push('missing metrics — fail closed');
    return audit;
  }

  const { success, observationWindowSeconds, autoStop, canaryPercent } = cfg;
  audit.canaryPercent = canaryPercent;
  audit.observationWindowSeconds = observationWindowSeconds;

  if (
    typeof metrics.observedSeconds === 'number' &&
    metrics.observedSeconds < observationWindowSeconds
  ) {
    audit.decision = 'halt';
    audit.reasons.push(
      `observation window incomplete: ${metrics.observedSeconds}s < ${observationWindowSeconds}s`,
    );
  }

  if (metrics.healthSuccessRate < success.minHealthSuccessRate) {
    audit.reasons.push(
      `healthSuccessRate ${metrics.healthSuccessRate} < ${success.minHealthSuccessRate}`,
    );
  }
  if (metrics.errorRate > success.maxErrorRate) {
    audit.reasons.push(
      `errorRate ${metrics.errorRate} > ${success.maxErrorRate}`,
    );
  }
  if (metrics.p95LatencyMs > success.maxP95LatencyMs) {
    audit.reasons.push(
      `p95LatencyMs ${metrics.p95LatencyMs} > ${success.maxP95LatencyMs}`,
    );
  }

  const failed = audit.reasons.length > 0;

  if (failed) {
    if (
      policy.manualOverride?.allowed &&
      typeof overrideReason === 'string' &&
      overrideReason.trim().length > 0
    ) {
      audit.decision = 'progress_override';
      audit.override = {
        reason: overrideReason.trim(),
        autoStopBypassed: true,
      };
      audit.reasons.push('manual override accepted with audit trail');
      return audit;
    }
    audit.decision = autoStop === false ? 'progress_auto_stop_disabled' : 'halt';
    return audit;
  }

  audit.decision = 'progress';
  return audit;
}

function arg(name) {
  const i = process.argv.indexOf(`--${name}`);
  if (i >= 0 && process.argv[i + 1]) return process.argv[i + 1];
  return undefined;
}

function main() {
  const target = arg('target');
  const metricsPath = arg('metrics');
  const overrideReason = arg('override');
  if (!target || !metricsPath) {
    console.error('Usage: --target backend|frontend --metrics <file> [--override reason]');
    process.exit(2);
  }
  const policy = loadPolicy();
  const metrics = JSON.parse(fs.readFileSync(metricsPath, 'utf8'));
  const audit = evaluateCanary({ policy, target, metrics, overrideReason });
  process.stdout.write(JSON.stringify(audit, null, 2) + '\n');
  if (audit.decision === 'halt') process.exit(1);
}

const isMain =
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);

if (isMain) main();
