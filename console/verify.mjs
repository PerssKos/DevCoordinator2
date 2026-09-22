// Browser verification of the Console: every primary view in every required
// state at a wide and a narrow viewport, with software-detectable checks
// (no document overflow, no clipped tiles, controls visible and enabled)
// and click-through proofs that controls call the real API and re-render.
//
// Usage:  CONSOLE_VERIFY_PLAYWRIGHT=<dir containing node_modules/playwright> \
//         CONSOLE_VERIFY_OUT=<dir> node console/verify.mjs
// Fixture data is used only here, never in production views.

import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import { appendFileSync } from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';

import { createEdge } from '../edge/devcoordinator2-edge.mjs';
import { createSessionManager } from '../edge/lib/session.mjs';
import { canonicalJson } from '../edge/lib/routes-store.mjs';
import { chooseRepository, revealTestRows, revealTestSettings, verifyTestsDesign } from './verify-tests-pane.mjs';
import { artifactResponse, verifyTestArtifacts } from './verify-artifacts.mjs';
import { verifyProgressCharts } from './verify-progress-charts.mjs';
import { performanceFixture, verifyPerformance } from './verify-performance.mjs';
import { verifyWorkspace } from './verify-workspace.mjs';
import { verifyAnnotations } from './verify-annotations.mjs';
import { verifyEvidenceLayout } from './verify-evidence-layout.mjs';
import { verifyAssetRefresh } from './verify-asset-refresh.mjs';

const BASE = 'example.test';
const HOST = process.env.CONSOLE_VERIFY_HOST || `console.${BASE}`;
const OUT = process.env.CONSOLE_VERIFY_OUT || path.join(os.tmpdir(), 'dc2-console-verify');
const pw = createRequire(path.join(process.env.CONSOLE_VERIFY_PLAYWRIGHT || process.cwd(), 'package.json'))('playwright');

const DEP = 'd0123456789abcdef';
const OBS = 'd3333333333333333';
const REPO = 'r0123456789abcdef';
const LONG = 'a-very-long-deployment-name-that-keeps-going-and-going-for-quite-a-while';
const V_EARLY = 'v0000000000000001'; const V_DONE = 'v0000000000000002';
const V_R1 = 'v0000000000000003'; const V_R2 = 'v0000000000000004';
const P_PAR = 'p1111111111111101'; const P_C1 = 'p1111111111111102';
const P_C2 = 'p1111111111111103'; const P_G1 = 'p1111111111111104';
const P_D1 = 'p1111111111111105'; const P_FB = 'p1111111111111106';
const P_LT = 'p1111111111111107'; const P_UNSIZED = 'p1111111111111108';
const P_UNSIZED_PARENT = 'p1111111111111109';
const TEST_RUN = 't20260101T000100Z-def456';
const ONE_PIXEL_PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk/x8AAusB9Y9Z4rUAAAAASUVORK5CYII=',
  'base64',
);

const progressFixture = (scenario, period = 'day') => {
  const spec = { hour: [3600000, 24], day: [86400000, 7], week: [604800000, 8] }[period];
  const [bucketMs, count] = spec;
  const referenceState = !!scenario.progressReference;
  const evidenceMissing = scenario.empty || referenceState;
  const alignedEnd = referenceState ? Date.UTC(2026, 8, 1) : Date.UTC(2026, 7, 31);
  const referenceTasks = [20, 0, 6, 3, 4, 6, 5];
  const referenceTasksAdded = [20, 2, 1, 0, 5, 0, 5];
  const referenceLines = [5500, 0, 1500, 0, 3500, 200, 2200];
  const referenceLinesAdded = [5500, 900, 250, 0, 1600, 0, 400];
  const partialTokens = [null, 120000, null, 0, 210000, 170000, 0];
  const series = Array.from({ length: count }, (_, index) => ({
    bucket_start_ms: alignedEnd - (count - index) * bucketMs,
    bucket_end_ms: alignedEnd - (count - index - 1) * bucketMs,
    tasks_completed: scenario.empty ? 0 : referenceState ? referenceTasks[index % referenceTasks.length] : [1, 0, 2, 1, 0, 2, 1, 1][index % 8],
    tasks_created: scenario.empty ? 0 : referenceState ? referenceTasksAdded[index % referenceTasksAdded.length] : [0, 1, 0, 0, 2, 0, 0, 1][index % 8],
    tasks_reopened: scenario.empty ? 0 : (index === count - 2 ? 1 : 0),
    planned_lines_completed: scenario.empty ? 0 : referenceState ? referenceLines[index % referenceLines.length] : [80, 0, 140, 95, 0, 220, 110, 75][index % 8],
    planned_lines_added: scenario.empty ? 0 : referenceState ? referenceLinesAdded[index % referenceLinesAdded.length] : [0, 40, 0, 0, 120, 0, 0, 30][index % 8],
    scope_lines_changed: scenario.empty ? 0 : [0, 40, 0, -20, 120, 0, 0, 30][index % 8],
    test_runs: evidenceMissing ? 0 : [3, 2, 4, 3, 5, 2, 4, 3][index % 8],
    tests_passed: evidenceMissing ? 0 : [3, 2, 3, 3, 4, 2, 4, 2][index % 8],
    test_pass_rate: evidenceMissing ? null : [1, 1, .75, 1, .8, 1, 1, .667][index % 8],
    total_tokens: evidenceMissing ? null : scenario.progressTokenPartial
      ? partialTokens[index % partialTokens.length]
      : [120000, 90000, 180000, 150000, 210000, 110000, 170000, 130000][index % 8],
    token_coverage: evidenceMissing || (scenario.progressTokenPartial
      && partialTokens[index % partialTokens.length] == null)
      ? 'unobserved' : ((scenario.partial && index === 2)
        || scenario.progressTokenPartial ? 'partial' : 'complete'),
  }));
  const currentTotals = {
    tasks_completed: series.reduce((sum, point) => sum + point.tasks_completed, 0),
    tasks_created: series.reduce((sum, point) => sum + point.tasks_created, 0),
    tasks_reopened: series.reduce((sum, point) => sum + point.tasks_reopened, 0),
    planned_lines_completed: series.reduce((sum, point) => sum + point.planned_lines_completed, 0),
    planned_lines_added: series.reduce((sum, point) => sum + point.planned_lines_added, 0),
    scope_lines_changed: series.reduce((sum, point) => sum + point.scope_lines_changed, 0),
    test_runs: series.reduce((sum, point) => sum + point.test_runs, 0),
    tests_passed: series.reduce((sum, point) => sum + point.tests_passed, 0),
    test_pass_rate: evidenceMissing ? null : .86,
    total_tokens: evidenceMissing ? null : series.reduce((sum, point) => sum + (point.total_tokens || 0), 0),
    tokens_per_completed_task: evidenceMissing ? null : 142500,
    tokens_per_planned_line: evidenceMissing ? null : 1220,
    tasks_completed_per_day: scenario.empty ? 0 : 1.1,
    tasks_created_per_day: scenario.empty ? 0 : .6,
    tests_per_completed_task: evidenceMissing ? null : 3.25,
  };
  const releaseWork = scenario.empty ? [] : referenceState ? [
    { task_id: P_C2, title: 'Check company internet names in the real app', status: 'in_progress', kind: 'goal', estimated_loc: 120, elaboration_needed: false, unblock_condition: 'TECHNICAL-UNBLOCK-MARKER internal acceptance harness', reopened: false, reopen_note: null },
    { task_id: P_UNSIZED, title: 'Check the basic rules that later features depend on', status: 'planned', kind: 'improvement', estimated_loc: null, elaboration_needed: false, unblock_condition: null, reopened: false, reopen_note: null },
    { task_id: P_G1, title: 'Run the finished work in the real app and check recovery', status: 'in_progress', kind: 'improvement', estimated_loc: 300, elaboration_needed: false, unblock_condition: null, reopened: true, reopen_note: 'TECHNICAL-REOPEN-MARKER atomic evidence reconciliation' },
  ] : [
    { task_id: P_C2, title: 'Help people recover from a failed sign-in', status: 'in_progress', kind: 'goal', estimated_loc: 275, elaboration_needed: false, unblock_condition: null, reopened: true, reopen_note: 'Final release receipts are not yet attached.' },
    { task_id: P_UNSIZED, title: 'Run the complete release in a real browser', status: 'planned', kind: 'improvement', estimated_loc: null, elaboration_needed: false, unblock_condition: null, reopened: false, reopen_note: null },
    { task_id: P_G1, title: 'Explain when an e-mail address is written incorrectly', status: 'planned', kind: 'stub', estimated_loc: 100, elaboration_needed: false, unblock_condition: null, reopened: false, reopen_note: null },
  ];
  return {
    repository_id: REPO, display_name: 'repo-one', period,
    generated_at_ms: referenceState ? Date.UTC(2026, 7, 31, 23, 59) : Date.UTC(2026, 7, 30, 23, 59),
    window: { bucket_ms: bucketMs, start_ms: alignedEnd - count * bucketMs,
      end_ms: referenceState ? Date.UTC(2026, 7, 31, 23, 59) : Date.UTC(2026, 7, 30, 23, 59),
      comparison_start_ms: alignedEnd - count * 2 * bucketMs, timezone: 'UTC' },
    scope: { tasks_total: scenario.empty ? 0 : referenceState ? 126 : 12,
      tasks_done: scenario.empty ? 0 : referenceState ? 32 : 7,
      planned_lines_total: scenario.empty ? 0 : referenceState ? 14200 : 2500,
      planned_lines_done: scenario.empty ? 0 : referenceState ? 7400 : 1558,
      unestimated_open_tasks: scenario.empty ? 0 : referenceState ? 71 : 1 },
    series,
    comparison: { current: currentTotals, previous: scenario.empty ? { ...currentTotals } : {
      ...currentTotals, tasks_completed: 6, tasks_created: 7, tasks_reopened: 0,
      planned_lines_completed: 920, planned_lines_added: 1080, scope_lines_changed: 80, test_runs: 20,
      tests_passed: 17, test_pass_rate: .82, total_tokens: 1300000,
      tokens_per_completed_task: 216667, tokens_per_planned_line: 1413,
      tasks_completed_per_day: .86, tasks_created_per_day: 1,
      tests_per_completed_task: 3.33 } },
    forecast: scenario.empty ? {
      state: 'unavailable', reason: 'no_planned_release', release: null,
      remaining_tasks: 0, remaining_planned_lines: 0, unestimated_tasks: 0,
      velocity: { tasks_per_day: 0, planned_lines_per_day: 0, lookback_days: 28 },
      target_date_recorded: false,
      explanation: 'Plan a release before estimating its delivery range.',
    } : referenceState ? {
      state: 'available', release: { release_id: V_R1, name: 'Current release', status: 'planned' },
      remaining_tasks: 94, remaining_planned_lines: 6800, unestimated_tasks: 71,
      velocity: { tasks: 32, planned_lines: 7400, lookback_days: 7,
        tasks_per_day: 4.6, planned_lines_per_day: 1057 },
      target_date_recorded: false, as_of_ms: Date.UTC(2026, 7, 31, 23, 59),
      likely_at_ms: Date.UTC(2026, 8, 13), earliest_at_ms: Date.UTC(2026, 8, 4),
      latest_at_ms: Date.UTC(2026, 8, 21), confidence_percent: 49,
      confidence: 'low', drivers: ['71 remaining tasks are not estimated'],
      explanation: '71 remaining tasks are not estimated. No target date is recorded.',
      assumptions: ['Unestimated work uses the median recorded task size when available.'],
    } : {
      state: 'available', release: { release_id: V_R1, name: 'Release 1', status: 'planned' },
      remaining_tasks: 5, remaining_planned_lines: 942, unestimated_tasks: 1,
      velocity: { tasks: 8, planned_lines: 1180, lookback_days: 14,
        tasks_per_day: .57, planned_lines_per_day: 84.3 },
      target_date_recorded: false, as_of_ms: Date.UTC(2026, 7, 30, 23, 59),
      likely_at_ms: Date.UTC(2026, 8, 5), earliest_at_ms: Date.UTC(2026, 8, 3),
      latest_at_ms: Date.UTC(2026, 8, 8), confidence_percent: 64,
      confidence: 'medium', drivers: ['1 remaining task is not estimated'],
      explanation: 'One remaining task is not estimated. No target date is recorded.',
      assumptions: ['Current task estimates are used as planned size.'],
    },
    release_work: releaseWork,
    coverage: { state: scenario.empty ? 'unavailable' : (scenario.partial || referenceState || scenario.progressTokenPartial) ? 'partial' : 'complete',
      plan: { state: 'complete', completed_with_estimate: 7, completed_total: 7 },
      tests: { state: evidenceMissing ? 'unobserved' : scenario.partial ? 'partial' : 'complete', recorded_runs: evidenceMissing ? 0 : 26, history_sources: evidenceMissing ? 0 : 1, unavailable_sources: 0, earliest_at: evidenceMissing ? null : '2026-08-01T00:00:00Z' },
      tokens: { state: scenario.empty ? 'unobserved' : referenceState || scenario.partial || scenario.progressTokenPartial ? 'partial' : 'complete', has_gaps: !!(scenario.partial || referenceState || scenario.progressTokenPartial), configured_collectors: 2, available_collectors: referenceState || scenario.progressTokenPartial ? 1 : 2, contributing_collectors: evidenceMissing ? 0 : scenario.progressTokenPartial ? 1 : 2, freshest_at_ms: evidenceMissing ? null : Date.UTC(2026, 7, 30, 23, 58), unavailable_reasons: scenario.progressTokenPartial ? { source_unavailable: 1 } : {}, ...scenario.progressTokenCoverage } },
    semantics: { tasks: 'terminal task status events in the permanent plan ledger', lines: 'current planned task estimates completed; not measured Git changes', lines_added: 'initial task estimates and estimate increases; estimate reductions and dropped work are excluded', tests: 'bounded repository-local terminal test summaries', tokens: 'provider total_tokens; missing collector coverage stays missing', forecast: 'deterministic range from recent pace, scope, estimates, and test stability' },
  };
};

const fixtures = (scenario) => {
  const runningState = scenario.applying ? 'applying' : (scenario.stopped ? 'stopped' : (scenario.serviceStopped ? 'degraded' : 'running'));
  const primaryRepositoryName = scenario.dashboardUsagePending ? 'active-project' : 'repo-one';
  const running = { deployment_id: DEP, repository_id: 'r0123456789abcdef', repository_name: primaryRepositoryName, name: 'web', source: 'worktree', state: runningState, domain: `app-dev.${BASE}`, public: false, current_generation: 17, updated_at: new Date(Date.now() - 90000).toISOString(), ttl_expires_at: null };
  const degraded = { deployment_id: 'd1111111111111111', repository_id: 'r0123456789abcdef', repository_name: primaryRepositoryName, name: LONG, source: 'checkout', state: 'degraded', domain: `${LONG}.${BASE}`, public: false, current_generation: 2147483647, updated_at: new Date().toISOString(), ttl_expires_at: '2026-12-31T00:00:00Z' };
  const observed = { deployment_id: OBS, repository_id: 'r9999999999999999', repository_name: 'legacy-repo', name: 'existing-compose-stack', source: 'observed', state: 'running', health: 'healthy', domain: `observed.${BASE}`, public: true, route_port: 5001, current_generation: null, updated_at: new Date().toISOString(), ttl_expires_at: null, observed_only: true };
  const observedComponents = [{ name: 'app', display_name: 'existing-compose-stack-app-1', type: 'container', state: 'running', health: 'healthy', generation: null, binding: { kind: 'observed-container', identity: 'd'.repeat(64) }, port: 5001, restarts: null, owned: false, independent_control: false, last_error: null }];
  const components = [
    { name: 'db', type: 'postgres', state: 'running', health: 'healthy', generation: 0, binding: { kind: 'container', identity: 'c'.repeat(64) }, port: 20001, restarts: 0, owned: true, independent_control: true, last_error: null },
    { name: 'api', type: 'process', state: scenario.stopped ? 'stopped' : 'running', health: scenario.stopped ? 'none' : 'healthy', generation: 17, binding: { kind: 'unit', identity: `devcoordinator2-deploy-${DEP}-api-g17.service` }, port: 20002, restarts: 3, owned: true, independent_control: true, last_error: null },
    { name: 'worker', type: 'process', state: 'failed', health: 'unhealthy', generation: 17, binding: { kind: 'unit', identity: `devcoordinator2-deploy-${DEP}-worker-g17.service` }, port: null, restarts: 9999999, owned: true, independent_control: false, last_error: 'exited 1: ' + 'x'.repeat(120) },
    { name: 'stack', type: 'compose', state: scenario.serviceStopped ? 'failed' : 'running', health: scenario.serviceStopped ? 'unhealthy' : 'healthy', generation: 17, binding: { kind: 'compose', identity: `dc2-${DEP}-stack` }, port: null, restarts: 0, owned: true, independent_control: true, last_error: null, services: [
      { name: 'bootstrap', role: 'finite', state: 'completed', containers: 1, independent: false },
      { name: 'stream-capture', role: 'running', state: 'running', containers: 1, independent: false },
      { name: 'projection-worker', role: 'running', state: scenario.serviceStopped ? 'stopped' : 'running', containers: 1, independent: true },
    ], completed_services: [{ service: 'bootstrap', generation: 17, exit_code: 0, recorded_at: '2026-08-28T00:00:00Z' }] },
    { name: 'smtp', type: 'external', state: 'running', health: 'healthy', generation: null, binding: { kind: null, identity: null }, port: null, restarts: 0, owned: false, independent_control: true, last_error: null },
  ];
  const points = Array.from({ length: 30 }, (_, i) => ({ minute: `2026-01-01T00:${String(i).padStart(2, '0')}Z`, min: i, avg: i * 1.5, max: i * 2, samples: 4 }));
  const denseUnsizedTasks = scenario.densePlan ? Array.from({ length: 110 }, (_, index) => ({
    task_id: `pdense${String(index).padStart(11, '0')}`,
    parent_task_id: null,
    release_id: V_R2,
    seq: 1000 + index,
    position: 100 + index,
    title: `Check customer journey ${index + 1}`,
    impact: 'This visible journey must work before the release is ready.',
    status: 'planned',
    kind: 'improvement',
    estimated_loc: null,
  })) : [];
  const usageHasNoMeasurements = scenario.empty || scenario.usageUnavailable;
  const usageCoverage = scenario.usageUnavailable
    ? { state: 'unavailable', has_gaps: true, configured_collectors: 4, available_collectors: 0, contributing_collectors: 0, freshest_at_ms: null, events: {}, token_observations: {}, unavailable_reasons: { source_unavailable: 4 }, database_schemas: [], taxonomy_versions: [] }
    : scenario.usageComplete
      ? { state: 'complete', has_gaps: false, configured_collectors: 4, available_collectors: 4, contributing_collectors: 4, freshest_at_ms: Date.now() - 120000, events: { complete: 82 }, token_observations: { complete: 147 }, unavailable_reasons: {}, database_schemas: [4], taxonomy_versions: [1] }
      : scenario.empty
        ? { state: 'unobserved', has_gaps: true, configured_collectors: 4, available_collectors: 4, contributing_collectors: 0, freshest_at_ms: null, events: {}, token_observations: {}, unavailable_reasons: {}, database_schemas: [4], taxonomy_versions: [1] }
        : { state: 'partial', has_gaps: true, configured_collectors: 4, available_collectors: 3, contributing_collectors: 3, freshest_at_ms: Date.now() - 120000, events: { complete: 80, partial: 2 }, token_observations: { complete: 144, partial: 3 }, unavailable_reasons: { source_unavailable: 1 }, database_schemas: [4], taxonomy_versions: [1] };
  const usageCompleteCoverage = { ...usageCoverage, state: 'complete', has_gaps: false,
    configured_collectors: 4, available_collectors: 4, contributing_collectors: 4,
    unavailable_reasons: {} };
  const usageUnobservedCoverage = { ...usageCoverage, state: 'unobserved', has_gaps: true,
    available_collectors: 3, contributing_collectors: 0, freshest_at_ms: null,
    events: {}, token_observations: {}, unavailable_reasons: {} };
  const usageMappingPendingCoverage = { ...usageCoverage, state: 'unavailable', has_gaps: true,
    available_collectors: 0, contributing_collectors: 0, freshest_at_ms: null,
    events: {}, token_observations: {}, unavailable_reasons: { mapping_pending: 4 },
    database_schemas: [], taxonomy_versions: [] };
  const usageSourceFailureCoverage = { ...usageMappingPendingCoverage,
    unavailable_reasons: { source_unavailable: 4 } };
  const usageIndexingCoverage = { ...usageMappingPendingCoverage,
    unavailable_reasons: { indexing: 4 }, snapshot: { updated_at_ms: null, refreshing: true, refresh_failed: false } };
  const usageSeries = usageHasNoMeasurements ? [] : Array.from({ length: 24 }, (_, index) => {
    const start = Date.UTC(2026, 7, 28, 19 + index);
    const phases = {
      planning: 45000 + index * 1200,
      implementation: 70000 + (index % 7) * 15000,
      testing: 30000 + (index % 5) * 9000,
      deployment: index % 6 === 0 ? 18000 : 7000,
      reporting: 5000 + (index % 4) * 2000,
      unattributed: 4000,
    };
    return { bucket_start_ms: start, bucket_end_ms: start + 3600000,
      coverage: scenario.usageComplete || index !== 17 ? 'complete' : 'partial', phases,
      total_tokens: Object.values(phases).reduce((sum, value) => sum + value, 0) };
  });
  const usageDetail = {
    repository_id: REPO, display_name: 'repo-one', range: '24h', generated_at_ms: Date.now(),
    coverage: usageCoverage,
    totals: usageHasNoMeasurements
      ? { total_tokens: null, input_tokens: null, cached_input_tokens: null, output_tokens: null, reasoning_tokens: null, model_requests: 0, tool_calls: 0, operations: 0 }
      : { total_tokens: 6405721, input_tokens: 6391772, cached_input_tokens: 6212608, output_tokens: 13949, reasoning_tokens: 7821, model_requests: 104, tool_calls: 236, operations: 340 },
    series: usageSeries,
    activities: usageHasNoMeasurements ? [] : [
      { phase: 'implementation', activity: 'coding', total_tokens: 2660000, share: .416, operations: 42, provenance: { agent_declared: 42 } },
      { phase: 'planning', activity: 'repository_analysis', total_tokens: 1740000, share: .272, operations: 28, provenance: { agent_declared: 28 } },
      { phase: 'testing', activity: 'integration_testing', total_tokens: 1240000, share: .194, operations: 35, provenance: { deterministic_classification: 35 } },
      { phase: 'deployment', activity: 'deployment', total_tokens: 540000, share: .084, operations: 9, provenance: { agent_declared: 9 } },
      { phase: 'reporting', activity: 'completion_handoff', total_tokens: 120000, share: .019, operations: 5, provenance: { agent_declared: 5 } },
      { phase: 'unattributed', activity: 'unknown', total_tokens: 70000, share: .011, operations: 8, provenance: { unknown: 8 } },
      { phase: 'unattributed', activity: 'mixed', total_tokens: 22000, share: .003, operations: 3, provenance: { unknown: 3 } },
      { phase: 'unattributed', activity: 'accounting_overhead', total_tokens: 13721, share: .002, operations: 12, provenance: { deterministic_classification: 12 } },
    ],
    time: {
      request_to_delivery: { measured_ms: usageHasNoMeasurements ? 0 : 472000, unknown_intervals: usageHasNoMeasurements ? 0 : 1 },
      execution_wall: { measured_ms: usageHasNoMeasurements ? 0 : 147000, unknown_intervals: usageHasNoMeasurements ? 0 : 2 },
      summed_agent_active: { measured_ms: usageHasNoMeasurements ? 0 : 349000, unknown_intervals: usageHasNoMeasurements ? 0 : 2 },
      phases: [],
    },
    tools: { outcomes: usageHasNoMeasurements ? [] : [
      { outcome: 'completed', count: 176 }, { outcome: 'failed', count: 19 },
      { outcome: 'interrupted', count: 21 }, { outcome: 'rejected', count: 12 },
      { outcome: 'unknown', count: 8 }], families: usageHasNoMeasurements ? [] : [
      { family: 'execution', count: 150 }, { family: 'coding', count: 64 }] },
    semantics: { tokens: 'provider total_tokens only; cached input and reasoning are subsets', time: 'wall, execution, phase, agent, and tool durations are separate', coverage: 'partial and unavailable collectors never contribute synthetic zeroes' },
  };
  return {
    'user.whoami': { local: false, identity: scenario.identity, user_id: 'u1', administrator: scenario.admin, grants: scenario.admin ? {} : { [DEP]: scenario.denied ? 'viewer' : 'operator' } },
    'deployment.list': { deployments: scenario.empty ? [] : [running, degraded, observed], declared: scenario.empty ? [] : [{ name: 'tool', source: 'worktree', deployment_id: 'd2222222222222222' }] },
    'deployment.status': { ...running, previous_generation: 16, route_port: 20002, route_component: 'api', components, log_dir: '/state/logs' },
    'deployment.observed-status': { ...observed, previous_generation: null, components: observedComponents, log_dir: null, native_project: 'existing-compose-stack', observation_source: 'legacy-current-import' },
    'deployment.logs': { component: 'api', tail: 'line 1\nline 2 ' + 'long '.repeat(60) + '\nline 3', truncated_before_tail: true, log_path: '/state/logs/api.log' },
    'health.history': { subject_kind: 'component', subject_id: `${DEP}/api`, metric: 'cpu_percent', minutes: 60, points: scenario.empty ? [] : points, truncated: false },
    'test.list': { runs: scenario.empty ? [] : [
      { proof: scenario.phaseTimings ? 'selected' : 'complete', selection: scenario.phaseTimings ? ['database-cases'] : [], targets: scenario.phaseTimings ? ['alpha', 'beta'] : [], phase_durations: scenario.phaseTimings ? [{phase:'build',duration_seconds:6,elapsed_seconds:4,checks:2},{phase:'cleanup',duration_seconds:0,elapsed_seconds:null,checks:1}] : [], checks: scenario.databasePhases ? [{name:'database-cases',display_name:'Database cases',status:'failed',phase:'case',cases:[{id:'missing-seed',status:'failed',duration_ms:250,phases:[{phase:'fixture',status:'failed',duration_ms:200},{phase:'case',status:'invalidated',duration_ms:0},{phase:'cleanup',status:'passed',duration_ms:50}]}]},{name:'blocked-consumer',display_name:'Skipped database check',phase:'case',status:'not_meaningful'}] : scenario.admission ? [{ name: 'compile', status: 'running', started_at: null, execution: { waiting: 1, admitted: 0, executing: 0, finished: 0, capacity_wait_ms: 0, process_duration_ms: 0 } }] : [], report_issue: scenario.reportIssue || null, run_id: 't20260101T000000Z-abc123', test: 'unit', requested_tier: 'pre-merge', readiness_eligible: false, status: scenario.testFinished ? 'passed' : 'running', started_at: new Date(Date.now() - (scenario.testFinished ? 3600000 : 0)).toISOString(), finished_at: scenario.testFinished ? new Date().toISOString() : null, duration_seconds: scenario.testFinished ? 3600 : null, exit_code: scenario.testFinished ? 0 : null, stdout_bytes_observed: 123456789, stderr_bytes_observed: 0, display_name: 'repo-one', worktree_path: '/srv/repos/repo-one', repository_id: REPO, worktree_id: 'w1', earlier_visual_evidence: scenario.earlierEvidence ? { run_id: 't20251231T000000Z-abc111', test: 'browser-journeys', started_at: '2025-12-31T00:00:00Z', visual_evidence: { status: 'available', bundle_count: 1, image_count: 8, issue_count: 0, issues_truncated: false } } : null, visual_evidence: scenario.evidenceWhileRunning ? { status: 'available', bundle_count: 1, image_count: 2, issue_count: 0, issues_truncated: false } : { status: 'unavailable', bundle_count: 0, image_count: 0, issue_count: 0, issues_truncated: false } },
      { run_id: TEST_RUN, test: 'ui-release', requested_tier: 'release', readiness_eligible: true, status: 'failed', started_at: new Date(Date.now() - 3600000).toISOString(), finished_at: new Date().toISOString(), duration_seconds: 3599.123, exit_code: 1, stdout_bytes_observed: 10, stderr_bytes_observed: 8388608, display_name: LONG, worktree_path: `/srv/repos/${LONG}`, repository_id: 'r2', worktree_id: 'w2', visual_evidence: { status: 'available', bundle_count: 1, image_count: 8, issue_count: 0, issues_truncated: false } }] },
    'test.capacity.get': {
      learned_capacity: 96, effective_capacity: 80, cap: 80, active: scenario.empty ? 0 : 52,
      waiting: scenario.empty ? 0 : 11, paused: false,
      last_adjustment: scenario.empty ? null : {
        event_id: 'e1', at: new Date(Date.now() - 300000).toISOString(), actor: 'scheduler',
        reason: 'underused_saturated_epoch', previous_capacity: 77, new_capacity: 96, cap: 80,
        p95_cpu_percent: 72.4, p95_memory_percent: 61.8, saturation_fraction: .84,
        epoch_seconds: 1840,
      },
    },
    'test.log.retention.get': { max_age_seconds: 86400, case_depth: 3, defaults: { max_age_seconds: 86400, case_depth: 3 }, updated_at: new Date().toISOString(), updated_by: 'schema-default', last_cleanup_at: new Date().toISOString(), last_cleanup_error_code: null },
    'test.log.catalog': { entries: scenario.empty || scenario.logEmpty ? [] : scenario.databasePhases ? ['fixture', 'case', 'cleanup'].map((phase) => ({
      log_ref: { run_id: 't20260101T000000Z-abc123', check: 'database-cases', phase, case: 'missing-seed', stream: 'stderr' },
      bytes: 100, lines: 2, complete: true, truncated: false, sha256: 'a'.repeat(64),
      structured_evidence: { available: false, formats: [], count: 0 },
    })) : [
      { log_ref: { run_id: 't20260101T000000Z-abc123', check: 'unit', phase: 'case', case: 'parser-17', stream: 'stderr' }, bytes: 8388608, lines: 42000, first_byte_at: new Date(Date.now() - 300000).toISOString(), last_byte_at: new Date().toISOString(), complete: true, truncated: false, sha256: 'a'.repeat(64), expires_at: new Date(Date.now() + 86400000).toISOString(), depth_rank: 1, structured_evidence: { available: true, formats: ['junit'], count: 2 } },
      { log_ref: { run_id: 't20260101T000000Z-abc123', phase: 'executor', stream: 'stdout' }, bytes: 220, lines: 2, first_byte_at: new Date(Date.now() - 300000).toISOString(), last_byte_at: new Date().toISOString(), complete: false, truncated: false, sha256: null, expires_at: null, depth_rank: null, structured_evidence: { available: false, formats: [], count: 0 } },
      { log_ref: { run_id: 't20260101T000000Z-abc123', check: 'structured', phase: 'check', stream: 'stdout' }, bytes: 180, lines: 1, first_byte_at: new Date(Date.now() - 300000).toISOString(), last_byte_at: new Date().toISOString(), complete: true, truncated: false, sha256: 'e'.repeat(64), expires_at: new Date(Date.now() + 86400000).toISOString(), depth_rank: 1, structured_evidence: { available: false, formats: [], count: 0 } },
    ], next_cursor: scenario.logCatalogPaged ? 'more-streams' : null },
    'test.log.tail': { segments: [{ line_start: scenario.logShortPaged ? 41999 : 41801, line_end: 42000, byte_start: 8340000, byte_end: 8388608,
      text: scenario.logShortPaged ? 'recent output 41999\nassertion failed at item_count=42000' : Array.from({ length: 200 }, (_, index) => {
        if (index === 196) return '2026-09-03T21:00:00Z warning retry=2 duration_ms=153.25';
        if (index === 197) return '<script>alert(1)</script> item_count=152';
        if (index === 198) return 'assertion failed';
        if (index === 199) return 'expected ready, actual pending';
        return `build step ${41801 + index} completed`;
      }).join('\n') }], next_cursor: 'older-tail', response_truncated: true },
    'test.log.search': { matches: [{ line_start: 41999, line_end: 41999, byte_start: 8388500, byte_end: 8388520, text: 'assertion failed' }], next_cursor: 'next-search', response_truncated: false },
    'test.log.range': { segments: [{ line_start: 40, line_end: 50, byte_start: 400, byte_end: 510, text: 'exact bounded range' }], next_cursor: null, response_truncated: false },
    'test.log.failure_context': { contexts: [{ line_start: 41999, line_end: 42000, byte_start: 8388500, byte_end: 8388608, text: 'assertion failed', occurrences: 2, fingerprint: `sha256:${'b'.repeat(64)}` }], next_cursor: null, response_truncated: false },
    'health.summary': { host: { cpu_percent: 93.4, memory_total: 264122252 * 1024, memory_used: 108579328 * 1024, memory_available: 155542924 * 1024, swap_total: 0, swap_used: 0, load_1: 8.32, load_5: 8.39, load_15: 7.69, fs_size: 2113513742336, fs_free: 148698841088, fs_used: 1964814901248, ncpu: 32, reconciliation: { managed_cpu_percent: 40.1, daemon_cpu_percent: 0.3, other_cpu_percent: 53.0, managed_memory: 50e9, daemon_memory: 120e6, other_memory: 60e9 } }, storage: { fs_used: 1964814901248, managed_repositories: 4e11, devcoordinator_state: 5e7, docker_shared: 3e10, docker_images: 2.7e10, docker_build_cache: 2.8e9, docker_shared_volumes: 1e8, other: 1.5e12 }, unhealthy_deployments: scenario.empty ? [] : [{ ...degraded, reasons: [{ component: 'worker', state: 'failed', detail: 'exited 1: boom' }, { component: 'api', state: 'stopped', detail: null }] }, { deployment_id: OBS, name: 'existing-compose-stack', source: 'observed', state: 'running', health: 'unhealthy', repository_name: 'legacy-repo', observed_only: true, reasons: [{ component: 'app', state: 'running', detail: 'container healthcheck failing (Up 3 days (unhealthy))' }] }], active_tests: scenario.empty ? [] : ['unit'], container_counts: { 'managed-test': 1, 'managed-preview': 0, 'managed-permanent': 3, 'orphaned-managed': 1, unmanaged: 43 }, alerts: scenario.empty ? [] : [{ alert_key: 'host/cpu', kind: 'host_cpu', severity: 'warning', message: 'host CPU 93% sustained', opened_at: new Date().toISOString() }, { alert_key: `component/${DEP}/worker/unhealthy`, kind: 'component_unhealthy', severity: 'critical', message: `component ${DEP}/worker is failed`, opened_at: new Date().toISOString() }], sampling: { retention_days: 30 } },
    'health.repositories': { repositories: scenario.empty ? [] : [{ repository_id: 'r9999999999999999', display_name: 'legacy-repo', root_path: '/srv/repos/legacy-repo', cpu_percent: 2.5, memory_bytes: 123456789, storage_bytes: 45678, storage: {}, health: 'healthy', deployments: [observed], trend_cpu: [2, 2, 3, 2, 2, 3, 2, 2, 3, 2, 2, 3], trend_memory: [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1] }, { repository_id: 'r0123456789abcdef', display_name: 'repo-one', root_path: '/srv/repos/repo-one', cpu_percent: 40.1, memory_bytes: 5e10, storage_bytes: 4e11, storage: {}, health: 'unhealthy', deployments: [running, degraded], trend_cpu: [1, 5, 3, 8, 2, 9, 4, 7, 3, 6, 2, 5], trend_memory: [1, 1, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5] }, { repository_id: 'r2', display_name: LONG, root_path: `/srv/repos/${LONG}`, cpu_percent: 0, memory_bytes: 0, storage_bytes: 1234567890123, storage: {}, health: 'none', deployments: [], trend_cpu: [], trend_memory: [] }], devcoordinator: { cpu_percent: 0.3, memory_bytes: 120e6, storage_bytes: 5e7 }, shared_unattributed: { cpu_percent: 53, memory_bytes: 60e9, storage: { docker_shared: 3e10, docker_images: 2.7e10, docker_build_cache: 2.8e9, docker_shared_volumes: 1e8, other: 1.5e12 } }, host: {} },
    'health.containers': { containers: scenario.empty ? [] : [
      { id: 'a'.repeat(64), name: 'devcoordinator2-deploy-x-db', image: 'postgres:16-alpine', state: 'running', status: 'Up 3 days', created: '2026-08-20 10:00:00 +0000 UTC', repository_id: 'r0123456789abcdef', deployment_id: DEP, component: 'db', run_id: null, caller_uid: 1000, client: 'claude', ttl_seconds: null, data: 'persistent', classification: 'managed-permanent', cpu_percent: 1.2, memory_bytes: 2677821440, pids: 7, container_layer_bytes: 0 },
      { id: 'b'.repeat(64), name: 'legacy-thing-1', image: 'some/image:latest', state: 'running', status: 'Up 6 weeks', created: '2026-07-01', repository_id: null, deployment_id: null, component: null, run_id: null, caller_uid: null, client: null, ttl_seconds: null, data: null, classification: 'unmanaged', cpu_percent: 12.5, memory_bytes: 9e9, pids: 100, container_layer_bytes: null },
      { id: 'c'.repeat(64), name: 'devcoordinator2-test-old-postgres', image: 'postgres:16-alpine', state: 'exited', status: 'Exited (0)', created: '2026-08-22', repository_id: 'r1', deployment_id: null, component: null, run_id: 't-old', caller_uid: 1001, client: 'codex', ttl_seconds: 3600, data: 'disposable', classification: 'orphaned-managed', cpu_percent: null, memory_bytes: null, pids: null, container_layer_bytes: 12345 },
      { id: 'd'.repeat(64), name: 'existing-compose-stack-app-1', image: 'app:1', state: 'running', status: 'Up 3 days (healthy)', created: '2026-08-20', repository_id: 'r0123456789abcdef', deployment_id: OBS, component: 'app', run_id: null, caller_uid: null, client: 'legacy-current-import', ttl_seconds: null, data: 'observed-only', classification: 'observed-current', cpu_percent: 2.5, memory_bytes: 123456789, pids: 4, container_layer_bytes: 45678 }], counts: { 'managed-test': 0, 'managed-preview': 0, 'managed-permanent': 1, 'observed-current': 1, 'orphaned-managed': 1, unmanaged: 1 } },
    'usage.repositories': { range: '24h', generated_at_ms: Date.now(), repositories: scenario.empty ? [] : [
      { repository_id: REPO, display_name: 'repo-one', range: '24h', coverage: scenario.usageIndexing ? usageIndexingCoverage : scenario.dashboardUsagePending ? usageMappingPendingCoverage : scenario.dashboardUsageUnobserved ? usageUnobservedCoverage : usageCoverage, total_tokens: scenario.usageIndexing || scenario.dashboardUsagePending || scenario.dashboardUsageUnobserved ? null : 6405721, model_requests: scenario.usageIndexing || scenario.dashboardUsagePending || scenario.dashboardUsageUnobserved ? 0 : 104, tool_calls: scenario.usageIndexing || scenario.dashboardUsagePending || scenario.dashboardUsageUnobserved ? 0 : 236, execution_wall_ms: scenario.usageIndexing || scenario.dashboardUsagePending || scenario.dashboardUsageUnobserved ? 0 : 147000 },
      { repository_id: 'r2', display_name: LONG, range: '24h', coverage: usageCompleteCoverage, total_tokens: 2100000, model_requests: 38, tool_calls: 74, execution_wall_ms: 72000 },
      { repository_id: 'r3', display_name: 'no-measurements', range: '24h', coverage: usageUnobservedCoverage, total_tokens: null, model_requests: 0, tool_calls: 0, execution_wall_ms: 0 },
      { repository_id: 'r4', display_name: 'not-connected', range: '24h', coverage: usageMappingPendingCoverage, total_tokens: null, model_requests: 0, tool_calls: 0, execution_wall_ms: 0 },
      { repository_id: 'r5', display_name: 'source-read-failed', range: '24h', coverage: usageSourceFailureCoverage, total_tokens: null, model_requests: 0, tool_calls: 0, execution_wall_ms: 0 }] },
    'usage.repository': usageDetail,
    'progress.repositories': { repositories: scenario.empty ? [] : [
      { repository_id: REPO, display_name: 'repo-one', open_tasks: 5,
        tasks_done: 7, planned_lines_done: 1558, planned_lines_total: 2500,
        next_release: { release_id: V_R1, name: 'Release 1', status: 'planned' } },
      { repository_id: 'r2', display_name: LONG, open_tasks: 0,
        tasks_done: 0, planned_lines_done: 0, planned_lines_total: 0,
        next_release: null }] },
    'progress.repository': progressFixture(scenario),
    'bug.list': { bugs: scenario.empty ? [] : [{ bug_id: 'b0123456789ab', component: 'api', summary: 'Returns 500 on /export when the report is large', expected: '200 with CSV', actual: '500', steps: '1. open /export 2. choose all-time 3. submit', opened_at: '2026-08-20T10:00:00Z', last_seen_at: new Date().toISOString(), occurrences: 42, reporter: 'dev@example.test', correlations: { deployment_id: DEP } }], store: '/bugs' },
    'user.list': { users: [{ user_id: 'u1', email: 'owner@example.test', administrator: true, grants: [], last_seen_at: new Date().toISOString() }, { user_id: 'u2', email: `${'verylongmailboxname'.repeat(3)}@example.test`, administrator: false, grants: [{ deployment_id: DEP, role: 'operator', granted_at: 't' }], last_seen_at: null }], invitations: [{ invitation_id: 'i1', email: 'new@example.test', administrator: false, grants: [{ deployment_id: DEP, role: 'viewer' }], created_at: 't', created_by: 'owner', expires_at: '2026-09-06T00:00:00Z' }], roles: ['access', 'viewer', 'operator', 'administrator'], owners: ['owner@example.test'] },
    'telegram.list': { configured: true, chats: [{ chat_id: 4242, email: 'owner@example.test', label: 'Owner', linked_at: 't', subscriptions: ['server', `deployment:${DEP}`] }], outbox_pending: 0, last_poll_at: new Date().toISOString(), last_error: null },
    ping: { daemon_version: '0.2.0', schema_version: 16, socket: '/run/x.sock' },
    'plan.overview': {
      repository_id: REPO, display_name: 'repo-one',
      releases: scenario.empty ? [] : [
        { release_id: V_EARLY, name: 'Early look', kind: 'preview', status: 'delivered', seq: 1, note: null, requested_at: '2026-08-20T10:00:00Z', delivered_at: '2026-08-20T12:00:00Z', url: null, port: 20005, tasks_total: 1, tasks_done: 1, loc_total: 150, loc_done: 150 },
        { release_id: V_DONE, name: 'First look', kind: 'preview', status: 'delivered', seq: 2, note: null, requested_at: '2026-08-21T10:00:00Z', delivered_at: '2026-08-21T12:00:00Z', url: `https://app-dev.${BASE}`, port: 20002, tasks_total: 1, tasks_done: 1, loc_total: 400, loc_done: 400 },
        { release_id: V_R1, name: 'Release 1', kind: 'release', status: 'planned', seq: 3, note: null, requested_at: null, delivered_at: null, url: null, port: null, tasks_total: 2, tasks_done: 0, loc_total: 300, loc_done: 0 },
        { release_id: V_R2, name: 'Release 2', kind: 'release', status: 'planned', seq: 4, note: null, requested_at: null, delivered_at: null, url: null, port: null, tasks_total: 1, tasks_done: 0, loc_total: 800, loc_done: 0 }],
      tasks: scenario.empty ? [] : [
        { task_id: 'p1111111111111100', parent_task_id: null, release_id: V_EARLY, seq: 0, position: 1, title: 'First greeting text', impact: null, status: 'done', kind: 'goal', estimated_loc: 150 },
        { task_id: P_D1, parent_task_id: null, release_id: V_DONE, seq: 1, position: 1, title: 'Show the welcome page', impact: null, status: 'done', kind: 'goal', estimated_loc: 400, elaboration_needed: true },
        { task_id: P_PAR, parent_task_id: null, release_id: V_R1, seq: 2, position: 1, title: 'Sign-in works', impact: 'Nobody can sign in yet.', status: 'planned', kind: 'goal', estimated_loc: null },
        { task_id: P_C1, parent_task_id: P_PAR, release_id: V_R1, seq: 3, position: 1, title: 'Sign-in form', impact: null, status: 'in_progress', kind: 'goal', estimated_loc: 300 },
        { task_id: P_G1, parent_task_id: P_C1, release_id: V_R1, seq: 4, position: 1, title: 'E-mail field checks its spelling', impact: null, status: 'planned', kind: 'stub', estimated_loc: 100 },
        { task_id: P_C2, parent_task_id: P_PAR, release_id: V_R1, seq: 5, position: 2, title: 'Wrong password message', impact: null, status: 'planned', kind: 'stub', estimated_loc: 200, technical_note: 'tech-note-marker-must-not-render' },
        { task_id: P_LT, parent_task_id: null, release_id: V_R2, seq: 6, position: 1, title: 'A very long task title that keeps going and going for quite a while so wrapping is exercised', impact: null, status: 'planned', kind: 'improvement', estimated_loc: 800 },
        { task_id: P_UNSIZED_PARENT, parent_task_id: null, release_id: V_R1, seq: 7, position: 0, title: 'Check the release before people use it', impact: 'The final checks must be easy to inspect as one group.', status: 'planned', kind: 'goal', estimated_loc: null },
        { task_id: P_UNSIZED, parent_task_id: P_UNSIZED_PARENT, release_id: V_R1, seq: 8, position: 1, title: 'Check the complete release in a real browser', impact: 'A release can look finished in code while visible journeys still fail.', status: 'planned', kind: 'improvement', estimated_loc: null },
        { task_id: P_FB, parent_task_id: null, release_id: null, seq: 9, position: 1, title: 'Make the export faster', impact: 'Big exports take minutes.', status: 'planned', kind: 'user_feedback', estimated_loc: 150 },
        ...denseUnsizedTasks],
      tasks_truncated: false, preview_requested: [],
      elaboration_requests: scenario.empty ? [] : [{ task_id: P_D1, title: 'Show the welcome page', outcome: 'Show the welcome page', status: 'done', kind: 'goal', requested_at: '2026-08-30T12:00:00Z' }],
      decisions: { unsummarized_count: scenario.empty ? 0 : 4, summary_due: false },
    },
    'plan.overview-list': { repositories: scenario.empty ? [] : [
      { repository_id: REPO, display_name: 'repo-one', root_path: '/srv/repos/repo-one', repository_source: { key: 'repo-one-origin', name: 'repo-one' }, open_tasks: 5, loc_done: 550, loc_total: 1800, current_release: { name: 'Release 1', kind: 'release', status: 'planned' }, preview_requested: false, elaboration_request_count: 1 },
      { repository_id: 'r2', display_name: LONG, open_tasks: 0, loc_done: 0, loc_total: 0, current_release: null, preview_requested: true, elaboration_request_count: 0 },
      ...(scenario.workspaceLongList ? [
        { repository_id: 'release-clone', display_name: '0.1.3-generated', root_path: '/srv/releases/0.1.3-generated', repository_source: { key: 'repo-one-origin', name: 'repo-one' } },
        ...Array.from({ length: 16 }, (_, index) => ({ repository_id: `workspace-${index}`, display_name: 'workspace', root_path: `/srv/owner/checkouts/independent-project-${index}/.local/daily/workspace` })),
      ] : [])] },
    'decision.tail': {
      repository_id: REPO, display_name: 'repo-one',
      summary: scenario.empty ? null : { body: 'The story so far: the app greets people plainly and exports are files.', covers_through_seq: 40, created_at: '2026-08-20T10:00:00Z' },
      decisions: scenario.empty ? [] : [
        { decision_id: 'n0000000000000041', seq: 41, ref: null, aspect: 'ui', title: 'Buttons were blue', body: 'Primary buttons used the blue accent.', technical_note: null, superseded_by: 'n0000000000000044', created_at: '2026-08-21T09:00:00Z', created_by: 'uid:1000' },
        { decision_id: 'n0000000000000042', seq: 42, ref: 'REPO-EXPORT-FILES', aspect: 'business_logic', title: 'Exports are downloadable files', body: 'People asked to keep their data, so every export produces a file they can save.', technical_note: 'csv via streaming writer', superseded_by: null, created_at: '2026-08-22T09:00:00Z', created_by: 'uid:1000' },
        { decision_id: 'n0000000000000043', seq: 43, ref: null, aspect: 'testing', title: 'Every page gets a browser test', body: 'Each page is exercised in a real browser before a release is delivered.', technical_note: null, superseded_by: null, created_at: '2026-08-23T09:00:00Z', created_by: 'uid:1000' },
        { decision_id: 'n0000000000000044', seq: 44, ref: null, aspect: 'ui', title: 'Buttons are green now', body: 'After trying the preview the owner preferred green buttons.', technical_note: null, superseded_by: null, created_at: '2026-08-24T09:00:00Z', created_by: 'uid:1000' }],
      has_more: !scenario.empty, unsummarized_count: scenario.empty ? 0 : 4, summary_due: false,
    },
    'decision.search': { repository_id: REPO, query: 'export', decisions: scenario.empty ? [] : [
      { decision_id: 'n0000000000000042', seq: 42, ref: 'REPO-EXPORT-FILES', aspect: 'business_logic', title: 'Exports are downloadable files', body: 'People asked to keep their data, so every export produces a file they can save.', technical_note: 'csv via streaming writer', superseded_by: null, created_at: '2026-08-22T09:00:00Z', created_by: 'uid:1000' }], has_more: false },
  };
};

const SCENARIOS = {
  populated: { identity: 'owner@example.test', admin: true },
  artifactFiles: { identity: 'owner@example.test', admin: true, artifactFiles: true, targetedOnly: true },
  empty: { identity: 'owner@example.test', admin: true, empty: true },
  error: { identity: 'owner@example.test', admin: true, error: true },
  loading: { identity: 'owner@example.test', admin: true, delayMs: 4000 },
  denied: { identity: 'dev@example.test', admin: false, denied: true },
  applying: { identity: 'owner@example.test', admin: true, applying: true },
  usageComplete: { identity: 'owner@example.test', admin: true, usageComplete: true, targetedOnly: true },
  usageUnavailable: { identity: 'owner@example.test', admin: true, usageUnavailable: true, targetedOnly: true },
  usageIndexing: { identity: 'owner@example.test', admin: true, usageIndexing: true, targetedOnly: true },
  dashboardUsagePending: { identity: 'owner@example.test', admin: true, dashboardUsagePending: true, targetedOnly: true },
  dashboardUsageUnobserved: { identity: 'owner@example.test', admin: true, dashboardUsageUnobserved: true, targetedOnly: true },
  progressPartial: { identity: 'owner@example.test', admin: true, partial: true, targetedOnly: true },
  progressTokenPartial: { identity: 'owner@example.test', admin: true, progressTokenPartial: true, targetedOnly: true },
  progressReference: { identity: 'owner@example.test', admin: true, progressReference: true, targetedOnly: true },
  logEmpty: { identity: 'owner@example.test', admin: true, logEmpty: true, targetedOnly: true },
  logCatalogPaged: { identity: 'owner@example.test', admin: true, logCatalogPaged: true, targetedOnly: true },
  logCatalogError: { identity: 'owner@example.test', admin: true, logCatalogError: true, targetedOnly: true },
  logReadError: { identity: 'owner@example.test', admin: true, logReadError: true, targetedOnly: true },
  logPageError: { identity: 'owner@example.test', admin: true, logPageError: true, targetedOnly: true },
  logShortPaged: { identity: 'owner@example.test', admin: true, logShortPaged: true, targetedOnly: true },
};
const VIEWS = ['#/deployments', `#/deployments/${DEP}`, '#/plan', `#/plan/${REPO}`, '#/progress', `#/progress/${REPO}`, '#/usage', `#/usage/${REPO}`, `#/performance/${REPO}`, '#/decisions', `#/decisions/${REPO}`, '#/tests', `#/tests/${TEST_RUN}`, '#/health', '#/health/containers', '#/bugs', '#/admin'];
const VIEWPORTS = { wide: { width: 1280, height: 800 }, narrow: { width: 390, height: 844 } };
const EVIDENCE_WIDE = { width: 1440, height: 1024 };
const destinationHref = (view) => {
  if (view.startsWith('#/deployments')) return '#/deployments';
  if (view.startsWith('#/plan')) return '#/plan';
  if (view.startsWith('#/progress')) return '#/progress';
  if (view.startsWith('#/usage')) return '#/usage';
  if (view.startsWith('#/performance')) return '#/performance';
  if (view.startsWith('#/decisions')) return '#/decisions';
  if (view.startsWith('#/tests')) return '#/tests';
  if (view.startsWith('#/health')) return '#/health';
  if (view.startsWith('#/bugs')) return '#/bugs';
  return '#/admin';
};
const PROJECT_DETAIL_VIEWS = new Set([`#/plan/${REPO}`, `#/progress/${REPO}`, `#/usage/${REPO}`, `#/performance/${REPO}`, `#/decisions/${REPO}`]);
const ADMIN_ONLY = ['health.summary', 'health.containers', 'health.container_remove', 'user.list', 'user.invite', 'user.remove', 'grant.set', 'grant.remove', 'test.list', 'test.start', 'test.stop', 'test.history', 'test.artifact.catalog', 'test.artifact.file', 'test.log.catalog', 'test.log.tail', 'test.log.search', 'test.log.range', 'test.log.failure_context', 'test.log.retention.get', 'test.log.retention.set', 'test.evidence.get', 'test.evidence.image', 'test.evidence.feedback.create', 'test.evidence.feedback.reply', 'test.evidence.feedback.edit', 'test.evidence.feedback.state', 'test.evidence.feedback.delete', 'test.capacity.get', 'test.capacity.set', 'deployment.apply', 'deployment.rollback', 'deployment.remove', 'deployment.set_domain', 'task.create', 'task.update', 'release.create', 'release.update', 'release.request', 'release.deliver', 'decision.record', 'decision.summarize'];
const OPERATOR_ONLY = ['usage.repositories', 'usage.repository', 'progress.repositories', 'progress.repository'];

async function startFakeDaemon(dir) {
  const socketPath = path.join(dir, 'daemon.sock');
  let scenario = SCENARIOS.populated;
  const calls = [];
  const repositoryPresentations = new Map();
  const settled = new WeakSet();
  const settledWaiters = new Set();
  const receivedWaiters = new Set();
  const delayedReplies = new Set();
  const mutable = { stopped: false, serviceStopped: false, taskUpdates: new Map(), createdTasks: [], previewRequested: false, failNextTaskUpdate: false, failNextLogCatalog: false, failNextLogRead: false, failNextLogPage: false, usageCollectionReads: 0, capacityCap: 80, logAge: 86400, logDepth: 3, evidenceImage: ONE_PIXEL_PNG, evidenceWidth: 1, evidenceHeight: 1, evidenceFeedback: [], feedbackSequence: 0 };
  const evidenceImage = (imageId, kind = 'viewport') => ({
    status: 'available', image_id: imageId, kind, mime: 'image/png',
    size: mutable.evidenceImage.length,
    sha256: crypto.createHash('sha256').update(mutable.evidenceImage).digest('hex'),
    width: mutable.evidenceWidth, height: mutable.evidenceHeight,
    captured_at: new Date(Date.now() - 120000).toISOString(),
  });
  const evidenceCell = (index, stateName, viewport, imageId, finding = null) => ({
    cell_id: `cell-${index}-${viewport.name}`,
    review_cell_key: String(index).repeat(64).slice(0, 64),
    plan_index: index,
    target_name: `Sign in [${stateName}]`, primary_journey: 'sign-in',
    state_name: stateName, requested_path: '/sign-in', final_path: '/sign-in',
    viewport, started_at: new Date(Date.now() - 180000).toISOString(),
    ended_at: new Date(Date.now() - 120000).toISOString(), duration_ms: 2100 + index,
    outcome: 'checked', http_status: 200, source_binding_status: 'matched',
    review: { status: 'review-required', decision: null },
    actions: [{ index: 0, action: 'click', outcome: 'completed', duration_ms: 16 }],
    findings: finding ? [{ severity: 'warning', rule: finding }] : [],
    screenshots: { viewport: evidenceImage(imageId), full_page: evidenceImage(`${imageId.slice(0, 63)}f`, 'full-page') },
  });
  const evidenceResult = () => ({
    repository_id: REPO, worktree_id: 'w1', run_id: TEST_RUN, status: 'available',
    bundles: [{ formal_run_id: 'formal-web-ui-fixture', generated_at: new Date().toISOString(),
      browser: 'playwright-managed-browser', check: 'formal-ui', phase: 'check', case: null,
      coverage: { checked_pages: 4, planned_pages: 4, failed: false, readiness_eligible: false },
      cells: [
        evidenceCell(0, 'base', { name: 'desktop', width: 1280, height: 800 }, '1'.repeat(64)),
        evidenceCell(0, 'base', { name: 'mobile', width: 390, height: 844 }, '2'.repeat(64)),
        evidenceCell(1, 'invalid-password', { name: 'desktop', width: 1280, height: 800 }, '3'.repeat(64), 'insufficient-text-contrast'),
        evidenceCell(1, 'invalid-password', { name: 'mobile', width: 390, height: 844 }, '4'.repeat(64), 'insufficient-text-contrast'),
      ] }], feedback: structuredClone(mutable.evidenceFeedback), issues: [],
    issues_truncated: false, image_count: 8,
  });
  const planOverview = () => {
    const result = fixtures(scenario)['plan.overview'];
    result.tasks = result.tasks
      .map((task) => ({ ...task, ...(mutable.taskUpdates.get(task.task_id) || {}) }))
      .filter((task) => task.status !== 'dropped')
      .concat(mutable.createdTasks);
    result.elaboration_requests = result.tasks.filter((task) => task.elaboration_needed).map((task) => ({
      task_id: task.task_id, title: task.title, outcome: task.outcome || task.title,
      status: task.status, kind: task.kind, requested_at: '2026-08-30T12:00:00Z',
    }));
    if (mutable.previewRequested) result.preview_requested = [{ release_id: 'v0000000000000099', name: 'Preview', requested_at: new Date().toISOString(), note: null }];
    return result;
  };
  const server = net.createServer({ allowHalfOpen: true }, (socket) => {
    socket.on('error', (error) => { if (!['EPIPE', 'ECONNRESET'].includes(error.code)) throw error; });
    let buf = '';
    socket.on('data', async (c) => {
      buf += c; if (!buf.endsWith('\n')) return;
      const req = JSON.parse(buf);
      const requestKeys = Object.keys(req).sort().join(',');
      if (req.protocol !== 2 || requestKeys !== 'client,id,operation,params,protocol'
        || typeof req.operation !== 'string' || !req.params || Array.isArray(req.params)
        || typeof req.params !== 'object') {
        socket.end(`${JSON.stringify({ protocol: 2, id: req.id || '', ok: false,
          error: { code: 'protocol_invalid', message: 'fixture requires protocol v2', detail: '' } })}\n`);
        return;
      }
      calls.push(req);
      for (const waiter of [...receivedWaiters]) {
        if (calls.length <= waiter.after) continue;
        receivedWaiters.delete(waiter);
        waiter.resolve(req);
      }
      const markSettled = () => {
        settled.add(req);
        for (const waiter of [...settledWaiters]) {
          if (!waiter.predicate(req)) continue;
          settledWaiters.delete(waiter);
          waiter.resolve(req);
        }
      };
      const reply = (payload) => {
        if (scenario.cacheState && payload.ok && ['usage.repository', 'usage.repositories', 'progress.repository'].includes(req.operation)) {
          payload = structuredClone(payload);
          const ready = !!req.params.wait_for_refresh;
          const failed = ['failed', 'unavailable'].includes(scenario.cacheState);
          const cold = scenario.cacheState === 'unavailable' || scenario.cacheState === 'loading' && !ready;
          const snapshot = { updated_at_ms: cold ? null : Date.UTC(2026, 8, 4, 12),
            refreshing: !ready && !failed, refresh_failed: failed };
          const items = req.operation === 'usage.repositories' ? payload.data.repositories : [payload.data];
          for (const item of items) {
            const coverage = req.operation === 'progress.repository' ? item.coverage.tokens : item.coverage;
            coverage.snapshot = snapshot;
            if (cold) {
              coverage.state = 'unavailable';
              coverage.available_collectors = 0;
              coverage.contributing_collectors = 0;
              if (item.totals) item.totals.total_tokens = null;
              if ('total_tokens' in item) item.total_tokens = null;
              for (const point of item.series || []) point.total_tokens = null;
            }
          }
        }
        socket.end(JSON.stringify({ protocol: 2, id: req.id, ...payload }) + '\n', markSettled);
      };
      if (scenario.delayMs) await new Promise((resolve) => delayedReplies.add(resolve));
      const cmd = req.operation;
      if ((scenario.cacheState || scenario.usageIndexing) && req.params.wait_for_refresh && !mutable.cacheReleased) await new Promise((resolve) => delayedReplies.add(resolve));
      if (cmd === 'user.whoami' && process.env.CONSOLE_VERIFY_RESET_PLAN_ON_SESSION === '1') {
        mutable.taskUpdates.clear();
        mutable.createdTasks.length = 0;
        mutable.previewRequested = false;
        mutable.failNextTaskUpdate = false;
      }
      if (scenario.error && cmd !== 'user.whoami') return reply({ ok: false, error: { code: 'internal_error', message: 'simulated daemon fault', detail: '' } });
      if (cmd === 'test.log.catalog' && mutable.failNextLogCatalog) {
        mutable.failNextLogCatalog = false;
        return reply({ ok: false, error: { code: 'log_unavailable', message: 'The log catalogue is temporarily unavailable.', detail: '' } });
      }
      if (cmd === 'test.log.catalog' && req.params.cursor === 'more-streams') return reply({ ok: true, data: {
        entries: [{ log_ref: { run_id: 't20260101T000000Z-abc123', check: 'lint', phase: 'check', stream: 'stdout' }, bytes: 48, lines: 1, first_byte_at: new Date(Date.now() - 300000).toISOString(), last_byte_at: new Date().toISOString(), complete: true, truncated: false, sha256: 'd'.repeat(64), expires_at: new Date(Date.now() + 86400000).toISOString(), depth_rank: 1, structured_evidence: { available: false, formats: [], count: 0 } }],
        next_cursor: null,
      } });
      if (cmd === 'test.log.tail' && mutable.failNextLogRead) {
        mutable.failNextLogRead = false;
        return reply({ ok: false, error: { code: 'log_expired', message: 'This retained log expired.', detail: '' } });
      }
      if (cmd === 'test.log.tail' && req.params.cursor && mutable.failNextLogPage) {
        mutable.failNextLogPage = false;
        return reply({ ok: false, error: { code: 'log_unavailable', message: 'Earlier output is temporarily unavailable.', detail: '' } });
      }
      if (scenario.denied && ADMIN_ONLY.includes(cmd)) return reply({ ok: false, error: { code: 'permission_denied', message: `${cmd} requires administrator`, detail: '' } });
      if (cmd.startsWith('performance.') && !scenario.admin) return reply({ ok:false, error:{code:'permission_denied',message:'Administrator access required',detail:''} });
      if (cmd.startsWith('performance.')) return reply({ok:true,data:performanceFixture(cmd,req.params,scenario)});
      if (scenario.denied && OPERATOR_ONLY.includes(cmd)) return reply({ ok: false, error: { code: 'permission_denied', message: `${cmd} requires operator`, detail: '' } });
      if (cmd === 'test.stop') { scenario = { ...scenario, testStopped: true }; return reply({ ok: true, data: { status: 'cancelled' } }); }
      if (cmd === 'test.start' && scenario.supersededRun) return reply({ ok: true, data: { run_id: 't20260101T000300Z-fed123', superseded_run_id: scenario.supersededRun } });
      if (cmd === 'test.list' && scenario.testPaged) {
        const data = fixtures(scenario)['test.list'];
        const page = req.params.after_worktree_id ? 1 : 0;
        return reply({ ok: true, data: { runs: [data.runs[page]], next_worktree_id: page ? null : data.runs[0].worktree_id } });
      }
      if (cmd === 'test.list' && scenario.testStopped) {
        const data = fixtures(scenario)['test.list'];
        data.runs[0] = { ...data.runs[0], status: 'cancelled', finished_at: new Date().toISOString() };
        return reply({ ok: true, data });
      }
      if (cmd === 'deployment.stop' && req.params.component === 'stack/projection-worker') { mutable.serviceStopped = true; return reply({ ok: true, data: { state: 'degraded' } }); }
      if (cmd === 'deployment.start' && req.params.component === 'stack/projection-worker') { mutable.serviceStopped = false; return reply({ ok: true, data: { state: 'running' } }); }
      if (cmd === 'deployment.stop') { mutable.stopped = true; return reply({ ok: true, data: { state: 'stopped' } }); }
      if (cmd === 'deployment.start') { mutable.stopped = false; return reply({ ok: true, data: { state: 'running' } }); }
      if (cmd === 'deployment.status' && req.params.deployment_id === OBS) return reply({ ok: true, data: fixtures({ ...scenario, stopped: mutable.stopped, serviceStopped: mutable.serviceStopped })['deployment.observed-status'] });
      if (cmd === 'deployment.set_domain') return reply({ ok: true, data: { deployment_id: req.params.deployment_id, domain: req.params.domain, public: !!req.params.public } });
      if (cmd === 'task.update') {
        if (mutable.failNextTaskUpdate) { mutable.failNextTaskUpdate = false; return reply({ ok: false, error: { code: 'simulated_failure', message: 'simulated task update failure', detail: '' } }); }
        const previous = mutable.taskUpdates.get(req.params.task_id) || {};
        const update = { ...previous };
        for (const key of ['estimated_loc', 'status', 'release_id', 'parent_task_id', 'position', 'title', 'outcome', 'elaboration_needed']) if (key in req.params) update[key] = req.params[key];
        mutable.taskUpdates.set(req.params.task_id, update);
        return reply({ ok: true, data: { task_id: req.params.task_id, ...update, state: 'done', status: update.status || 'planned' } });
      }
      if (cmd === 'task.create') {
        const task = { task_id: `pnew${String(mutable.createdTasks.length + 1).padStart(13, '0')}`, parent_task_id: null, release_id: null, seq: 100 + mutable.createdTasks.length, position: 100 + mutable.createdTasks.length, title: req.params.title, impact: req.params.impact || null, status: 'planned', kind: req.params.kind, estimated_loc: null, elaboration_needed: false };
        mutable.createdTasks.push(task);
        return reply({ ok: true, data: { task_id: task.task_id, status: task.status } });
      }
      if (cmd === 'release.request') { mutable.previewRequested = true; return reply({ ok: true, data: { status: 'requested' } }); }
      if (cmd === 'test.capacity.set') {
        mutable.capacityCap = req.params.cap;
        const learned = 96;
        return reply({ ok: true, data: {
          ...fixtures(scenario)['test.capacity.get'], cap: mutable.capacityCap,
          effective_capacity: mutable.capacityCap == null ? learned : Math.min(learned, mutable.capacityCap),
          last_adjustment: { event_id: 'e2', at: new Date().toISOString(), actor: 'administrator', reason: 'administrator_cap_changed', previous_capacity: learned, new_capacity: learned, cap: mutable.capacityCap, p95_cpu_percent: null, p95_memory_percent: null, saturation_fraction: null, epoch_seconds: null },
        } });
      }
      if (cmd === 'test.log.retention.get') return reply({ ok: true, data: { ...fixtures(scenario)['test.log.retention.get'], max_age_seconds: mutable.logAge, case_depth: mutable.logDepth } });
      if (cmd === 'test.log.retention.set') {
        mutable.logAge = req.params.max_age_seconds; mutable.logDepth = req.params.case_depth;
        return reply({ ok: true, data: { ...fixtures(scenario)['test.log.retention.get'], max_age_seconds: mutable.logAge, case_depth: mutable.logDepth, cleanup_requested: true } });
      }
      if (cmd === 'test.log.tail' && req.params.check === 'structured') return reply({ ok: true, data: {
        segments: [{ line_start: 1, line_end: 1, byte_start: 0, byte_end: 180,
          text: '{"summary":{"passed":12,"failed":0},"duration_ms":83.5,"message":"<img src=x onerror=alert(1)>","complete":true}' }],
        next_cursor: null, response_truncated: false,
      } });
      if (cmd === 'test.log.tail' && req.params.phase === 'executor') return reply({ ok: true, data: {
        segments: [{ line_start: 1, line_end: 2, byte_start: 0, byte_end: 220,
          text: '{"event":"executor finished successfully","count":42,"ok":true,"duration_ms":153.25,"at":"2026-09-03T21:00:00Z"}\n{"event":"artifact","bytes":2048,"cached":false,"status":"passed"}' }],
        next_cursor: null, response_truncated: false,
      } });
      if (cmd === 'test.log.tail' && req.params.cursor === 'older-tail') return reply({ ok: true, data: {
        segments: [{ line_start: 41601, line_end: 41800, byte_start: 8290000, byte_end: 8340000,
          text: Array.from({ length: 200 }, (_, index) => `earlier setup output ${index + 1}`).join('\n') }],
        next_cursor: 'oldest-tail', response_truncated: true,
      } });
      if (cmd === 'test.log.tail' && req.params.cursor === 'oldest-tail') return reply({ ok: true, data: {
        segments: [{ line_start: 41401, line_end: 41600, byte_start: 8240000, byte_end: 8290000,
          text: Array.from({ length: 200 }, (_, index) => `oldest retained output ${index + 1}`).join('\n') }],
        next_cursor: null, response_truncated: false,
      } });
      if (cmd === 'test.log.search' && req.params.cursor === 'next-search') return reply({ ok: true, data: {
        matches: [{ line_start: 41000, line_end: 41000, byte_start: 8200000, byte_end: 8200024, text: 'pending state persisted' }],
        next_cursor: null, response_truncated: false,
      } });
      const artifacts = artifactResponse(cmd, req.params, scenario);
      if (artifacts) return reply(artifacts);
      if (cmd === 'test.evidence.lookup') {
        if (scenario.identity !== 'owner@example.test') return reply({ ok: false, error: { code: 'permission_denied', message: 'Administrator access is required.', detail: '' } });
        const retained = req.params.run_id === 't20251101T000000Z-abcd12';
        const earlier = scenario.earlierEvidence && req.params.run_id === 't20251231T000000Z-abc111';
        if (scenario.empty || !retained && !earlier && req.params.run_id !== TEST_RUN || req.params.image_id && !/^[1-8]{64}$/.test(req.params.image_id)) return reply({ ok: false, error: { code: 'test_evidence_expired', message: 'The selected visual evidence is unavailable or has expired.', detail: '' } });
        return reply({ ok: true, data: {
          context: { repository_id: earlier ? REPO : 'r2', worktree_id: earlier ? 'w1' : 'w2', worktree_path: earlier ? '/srv/repos/repo-one' : `/srv/repos/${LONG}`, display_name: earlier ? 'repo-one' : LONG, run_id: req.params.run_id, test: retained ? 'retained-visual-review' : earlier ? 'browser-journeys' : 'ui-release', started_at: '2025-11-01T00:00:00Z' },
          evidence: { ...evidenceResult(), run_id: req.params.run_id },
        } });
      }
      if (cmd === 'test.evidence.get') return reply({ ok: true, data: evidenceResult() });
      if (cmd === 'test.evidence.image') {
        const start = req.params.offset || 0;
        const end = Math.min(mutable.evidenceImage.length, start + (req.params.max_bytes || 184320));
        return reply({ ok: true, data: {
          image_id: req.params.image_id, mime: 'image/png',
          sha256: crypto.createHash('sha256').update(mutable.evidenceImage).digest('hex'),
          total_bytes: mutable.evidenceImage.length, offset: start, bytes: end - start,
          base64: mutable.evidenceImage.subarray(start, end).toString('base64'),
          next_offset: end < mutable.evidenceImage.length ? end : null,
        } });
      }
      if (cmd === 'test.evidence.feedback.create') {
        mutable.feedbackSequence += 1;
        const feedbackId = `f${String(mutable.feedbackSequence).padStart(16, '0')}`;
        const taskId = `pvisual${String(mutable.feedbackSequence).padStart(10, '0')}`;
        const commentId = `mroot${String(mutable.feedbackSequence).padStart(11, '0')}`;
        const now = new Date().toISOString();
        const feedback = {
          feedback_id: feedbackId, task_id: taskId, task_status: 'planned', state: 'open',
          run_id: TEST_RUN, check: 'formal-ui', phase: 'check', case: null,
          formal_run_id: 'formal-web-ui-fixture', cell_id: 'cell-1-desktop',
          review_cell_key: '1'.repeat(64), image_id: req.params.image_id,
          screenshot_kind: 'viewport', screenshot_sha256: crypto.createHash('sha256').update(mutable.evidenceImage).digest('hex'),
          marks: structuredClone(req.params.marks), author: scenario.identity,
          created_at: now, updated_at: now, can_delete: true,
          comments: [{ comment_id: commentId, body: req.params.body, author: scenario.identity,
            created_at: now, updated_at: now, deleted: false, can_edit: true }],
          comments_truncated: false,
        };
        mutable.evidenceFeedback.push(feedback);
        mutable.createdTasks.push({ task_id: taskId, parent_task_id: null, release_id: null,
          seq: 200 + mutable.feedbackSequence, position: 200 + mutable.feedbackSequence,
          title: `Review: ${req.params.body}`, impact: 'The tested page needs visual attention.',
          status: 'planned', kind: 'user_feedback', estimated_loc: null,
          elaboration_needed: false });
        return reply({ ok: true, data: { task_id: taskId, feedback_id: feedbackId,
          position: 200 + mutable.feedbackSequence, feedback: structuredClone(feedback) } });
      }
      if (cmd.startsWith('test.evidence.feedback.')) {
        const feedback = mutable.evidenceFeedback.find((item) => item.feedback_id === req.params.feedback_id);
        if (!feedback) return reply({ ok: false, error: { code: 'args_invalid', message: 'feedback missing', detail: '' } });
        const now = new Date().toISOString();
        if (cmd.endsWith('.reply')) {
          feedback.comments.push({ comment_id: `mreply${String(feedback.comments.length).padStart(10, '0')}`,
            body: req.params.body, author: scenario.identity, created_at: now,
            updated_at: now, deleted: false, can_edit: true });
        } else if (cmd.endsWith('.edit')) {
          const comment = feedback.comments.find((item) => item.comment_id === req.params.comment_id);
          if (comment) { comment.body = req.params.body; comment.updated_at = now; }
        } else if (cmd.endsWith('.state')) {
          feedback.state = req.params.state; feedback.task_status = req.params.state === 'resolved' ? 'done' : 'planned';
        } else if (cmd.endsWith('.delete')) {
          feedback.state = 'deleted'; feedback.task_status = 'dropped'; feedback.can_delete = false;
        }
        feedback.updated_at = now;
        return reply({ ok: true, data: { feedback: structuredClone(feedback) } });
      }
      if (['deployment.restart', 'deployment.apply', 'deployment.rollback', 'deployment.remove', 'bug.report', 'bug.close', 'user.invite', 'user.remove', 'grant.set', 'grant.remove', 'telegram.link', 'telegram.subscribe', 'telegram.unsubscribe', 'test.stop', 'test.start', 'health.container_remove'].includes(cmd)) return reply({ ok: true, data: { state: 'done', status: 'done' } });
      if (cmd === 'repository.presentation.update') {
        if (!scenario.admin) return reply({ ok: false, error: { code: 'permission_denied', message: 'Administrator access required' } });
        repositoryPresentations.set(req.params.repository_id, structuredClone(req.params));
        return reply({ ok: true, data: req.params });
      }
      if (cmd === 'plan.overview' && !req.params.repository_id) {
        const result = fixtures(scenario)['plan.overview-list'];
        result.repositories = result.repositories.map((record) => ({ ...record, presentation: repositoryPresentations.get(record.repository_id) || null }));
        return reply({ ok: true, data: result });
      }
      if (cmd === 'plan.overview') return reply({ ok: true, data: planOverview() });
      if (cmd === 'progress.repository') return reply({ ok: true, data: progressFixture(scenario, req.params.period || 'day') });
      if (cmd === 'progress.repositories') return reply({ ok: true, data: fixtures(scenario)['progress.repositories'] });
      if (cmd === 'usage.repository') {
        if (scenario.dashboardUsagePending) await new Promise((resolve) => delayedReplies.add(resolve));
        const result = structuredClone(fixtures({ ...scenario, stopped: mutable.stopped,
          serviceStopped: mutable.serviceStopped })['usage.repository']);
        result.range = req.params.range || '24h';
        const count = result.range === '30d' ? 30 : result.range === '7d' ? 28 : 24;
        const step = result.range === '30d' ? 86400000 : result.range === '7d' ? 21600000 : 3600000;
        if (result.series.length) result.series = Array.from({ length: count }, (_, index) => {
          const source = result.series[index % result.series.length];
          const start = Date.UTC(2026, 7, 29) - (count - index) * step;
          return { ...source, bucket_start_ms: start, bucket_end_ms: start + step };
        });
        return reply({ ok: true, data: result });
      }
      if (cmd === 'usage.repositories') {
        const fixtureScenario = scenario.usageIndexing && mutable.usageCollectionReads > 0
          ? { ...scenario, usageIndexing: false } : scenario;
        mutable.usageCollectionReads += 1;
        const result = structuredClone(fixtures(fixtureScenario)['usage.repositories']);
        result.range = req.params.range || '24h';
        return reply({ ok: true, data: result });
      }
      if (cmd === 'test.capacity.get') {
        const result = structuredClone(fixtures(scenario)['test.capacity.get']);
        result.cap = mutable.capacityCap;
        result.effective_capacity = mutable.capacityCap == null ? result.learned_capacity : Math.min(result.learned_capacity, mutable.capacityCap);
        return reply({ ok: true, data: result });
      }
      if (cmd === 'decision.tail' && req.params.repository_id === 'r9999999999999999') {
        return reply({ ok: true, data: { repository_id: req.params.repository_id, display_name: 'legacy-repo', summary: null, decisions: [], has_more: false, unsummarized_count: 0, summary_due: false } });
      }
      const data = fixtures({ ...scenario, stopped: mutable.stopped, serviceStopped: mutable.serviceStopped })[cmd];
      if (data === undefined) return reply({ ok: false, error: { code: 'operation_unknown', message: cmd, detail: '' } });
      return reply({ ok: true, data: data });
    });
  });
  await new Promise((r) => server.listen(socketPath, r));
  return {
    socketPath,
    calls,
    setScenario: (s) => { for (const release of delayedReplies) release(); delayedReplies.clear(); scenario = s; mutable.cacheReleased = false; mutable.stopped = false; mutable.serviceStopped = false; mutable.taskUpdates.clear(); mutable.createdTasks.length = 0; mutable.previewRequested = false; mutable.failNextTaskUpdate = false; mutable.failNextLogCatalog = !!s.logCatalogError; mutable.failNextLogRead = !!s.logReadError; mutable.failNextLogPage = !!s.logPageError; mutable.usageCollectionReads = 0; mutable.capacityCap = 80; mutable.logAge = 86400; mutable.logDepth = 3; mutable.evidenceFeedback.length = 0; mutable.feedbackSequence = 0; calls.length = 0; },
    setEvidenceImage: (bytes, width, height) => { mutable.evidenceImage = Buffer.from(bytes); mutable.evidenceWidth = width; mutable.evidenceHeight = height; },
    failNextTaskUpdate: () => { mutable.failNextTaskUpdate = true; },
    releaseDelayed: () => { mutable.cacheReleased = true; for (const release of delayedReplies) release(); delayedReplies.clear(); },
    waitForReceivedAfter: (after) => {
      if (calls.length > after) return Promise.resolve(calls[after]);
      return new Promise((resolve) => receivedWaiters.add({ after, resolve }));
    },
    waitForCall: (predicateOrCommand) => {
      const predicate = typeof predicateOrCommand === 'string'
        ? (call) => call.operation === predicateOrCommand : predicateOrCommand;
      const existing = calls.find((call) => settled.has(call) && predicate(call));
      if (existing) return Promise.resolve(existing);
      return new Promise((resolve, reject) => {
        const deadline = AbortSignal.timeout(30000);
        const waiter = { predicate, resolve: (value) => { deadline.removeEventListener('abort', onTimeout); resolve(value); } };
        const onTimeout = () => {
          settledWaiters.delete(waiter);
          reject(new Error(`Fixture event deadline exceeded: ${String(predicateOrCommand)}; recent operations: ${calls.slice(-8).map(call => call.operation).join(', ')}`));
        };
        deadline.addEventListener('abort', onTimeout, { once: true });
        settledWaiters.add(waiter);
      });
    },
    close: () => new Promise((r) => server.close(r)),
  };
}

async function waitForRenderFrame(page) {
  await page.evaluate(() => new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));
}

async function waitForSettledCall(daemon, page, predicateOrCommand) {
  try { await daemon.waitForCall(predicateOrCommand); }
  catch (error) { await page.screenshot({ path: path.join(OUT, 'event-deadline.png'), fullPage: true }); throw error; }
  await waitForRenderFrame(page);
}

async function main() {
  await fs.mkdir(OUT, { recursive: true });
  const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-verify-'));
  const daemon = await startFakeDaemon(tmp);
  const payload = { generation: 1, published_at: 'x', domain: BASE, routes: [], access: { owners: ['owner@example.test'], grants: [] } };
  await fs.writeFile(path.join(tmp, 'routes.json'), JSON.stringify({ schema: 1, payload_sha256: crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex'), ...payload }));
  const secret = 'console-verify-secret-0123456789';
  const edge = await createEdge({ baseDomain: BASE, consoleHost: HOST, httpPort: 0, httpOnly: true, sessionSecret: secret, oidcIssuer: 'http://127.0.0.1:1/', oidcClientId: '', oidcClientSecret: '', routesFile: path.join(tmp, 'routes.json'), stateDir: path.join(tmp, 'edge-state'), daemonSocket: daemon.socketPath, consoleDir: path.resolve(path.dirname(new URL(import.meta.url).pathname)) }, { log: { info() {}, warn() {}, error: (...a) => console.error('edge', ...a), debug() {} } });
  const [port] = await edge.listen();
  const sessions = createSessionManager({ secret, ttlMs: 3600000, cookieName: 'dc2_session', cookieDomain: `.${BASE}`, secure: false });
  const holdFile = process.env.CONSOLE_VERIFY_HOLD_FILE;
  if (holdFile) {
    const scenarioName = process.env.CONSOLE_VERIFY_HOLD_SCENARIO || 'populated';
    const scenario = SCENARIOS[scenarioName];
    if (!scenario) throw new Error(`unknown hold scenario ${scenarioName}`);
    daemon.setScenario(scenario);
    const { cookie } = sessions.issue({ sub: 'sub', email: scenario.identity, name: 'Verifier' });
    const holdView = process.env.CONSOLE_VERIFY_HOLD_VIEW || `#/deployments/${DEP}`;
    const targetUrl = `http://${HOST}:${port}/${holdView}`;
    if (holdView.startsWith('#/tests/')) {
      const captureBrowser = await pw.chromium.launch({ args: [`--host-resolver-rules=MAP *.${BASE} 127.0.0.1`] });
      const captureContext = await captureBrowser.newContext({ viewport: EVIDENCE_WIDE });
      await captureContext.addCookies([{ name: 'dc2_session', value: cookie.split(';')[0].split('=')[1], domain: `.${BASE}`, path: '/' }]);
      const capturePage = await captureContext.newPage();
      await capturePage.goto(`http://${HOST}:${port}/#/tests`);
      await capturePage.waitForSelector('.test-results');
      await capturePage.evaluate(() => document.querySelector('#toasts')?.replaceChildren());
      daemon.setEvidenceImage(
        await capturePage.screenshot({ type: 'png' }),
        EVIDENCE_WIDE.width,
        EVIDENCE_WIDE.height,
      );
      await captureBrowser.close();
    }
    let previewServer = null;
    let previewUrl = targetUrl;
    if (process.env.CONSOLE_VERIFY_SHARE_PREVIEW === '1') {
      const [sessionCookie] = cookie.split(';');
      previewServer = http.createServer((request, response) => {
        const upstream = http.request({
          host: '127.0.0.1', port, method: request.method, path: request.url,
          headers: { ...request.headers, host: HOST, cookie: sessionCookie },
        }, (incoming) => {
          response.writeHead(incoming.statusCode || 502, {
            ...incoming.headers, 'cache-control': 'no-store',
          });
          incoming.pipe(response);
        });
        upstream.on('error', (error) => {
          if (!response.headersSent) response.writeHead(502, { 'content-type': 'text/plain' });
          response.end(`Preview proxy unavailable: ${error.message}`);
        });
        request.pipe(upstream);
      });
      await new Promise((resolve) => previewServer.listen(0, '127.0.0.1', resolve));
      previewUrl = `http://127.0.0.1:${previewServer.address().port}/${holdView}`;
    }
    const receipt = {
      url: previewUrl,
      target_url: targetUrl,
      cookie: cookie.split(';')[0],
      scenario: scenarioName,
    };
    await fs.writeFile(holdFile, JSON.stringify(receipt));
    console.log(JSON.stringify({ fixture: holdFile, ...receipt, cookie: 'redacted' }));
    await new Promise((resolve) => {
      process.once('SIGINT', resolve);
      process.once('SIGTERM', resolve);
    });
    if (previewServer) await new Promise((resolve) => previewServer.close(resolve));
    await edge.close();
    await daemon.close();
    return;
  }
  const browser = await pw.chromium.launch({ args: [`--host-resolver-rules=MAP *.${BASE} 127.0.0.1`] });
  const report = { checks: [], failures: [] };
  const check = (name, ok, detail = '') => { report.checks.push({ name, ok, detail }); appendFileSync(path.join(OUT, 'checks.jsonl'), `${JSON.stringify({ name, ok, detail })}\n`); if (!ok) report.failures.push(`${name}: ${detail}`); };
  const appSource = await fs.readFile(new URL('./app.js', import.meta.url), 'utf8');
  check('administrator controls have no native or data-driven confirmation path',
    !/window\.confirm|data-confirm|data-delete-data/.test(appSource));

  if (process.env.CONSOLE_VERIFY_ASSET_REFRESH_ONLY) {
    try { await verifyAssetRefresh({ browser, check }); }
    catch (error) { check('ordinary reload acceptance', false, error.message); }
    finally {
      await fs.writeFile(path.join(OUT, 'report.json'), JSON.stringify(report, null, 2));
      await browser.close(); await edge.close(); await daemon.close();
    }
    console.log(JSON.stringify({ checks: report.checks.length, failures: report.failures }));
    process.exitCode = report.failures.length ? 1 : 0;
    return;
  }

  if (process.env.CONSOLE_VERIFY_PROGRESS_ONLY) {
    try {
      const context = await browser.newContext({ viewport: VIEWPORTS.wide, hasTouch: true, colorScheme: process.env.CONSOLE_VERIFY_THEME || 'light' });
      const { cookie } = sessions.issue({ sub: 'sub', email: 'owner@example.test' });
      await context.addCookies([{ name: 'dc2_session', value: cookie.split(';')[0].split('=')[1], domain: '.' + BASE, path: '/' }]);
      const page = await context.newPage();
      page.setDefaultTimeout(8000);
      try {
        await verifyProgressCharts({ page, daemon, check, scenario: SCENARIOS.progressReference,
          baseUrl: `http://${HOST}:${port}/`, output: OUT,
          settle: (period) => waitForSettledCall(daemon, page,
            (call) => call.operation === 'progress.repository' && call.params.period === period) });
      } catch (error) { check('Progress chart verification', false, error.message); }
      await context.close();
      await fs.writeFile(path.join(OUT, 'report.json'), JSON.stringify(report, null, 2));
      console.log(JSON.stringify({ checks: report.checks.length, failures: report.failures, report: path.join(OUT, 'report.json') }));
      process.exitCode = report.failures.length ? 1 : 0;
    } finally { await browser.close(); await edge.close(); await daemon.close(); }
    return;
  }

  if (process.env.CONSOLE_VERIFY_PERFORMANCE_ONLY || process.env.CONSOLE_VERIFY_TESTS_DESIGN_ONLY || process.env.CONSOLE_VERIFY_ARTIFACTS_ONLY || process.env.CONSOLE_VERIFY_WORKSPACE_ONLY || process.env.CONSOLE_VERIFY_ANNOTATIONS_ONLY || process.env.CONSOLE_VERIFY_EVIDENCE_LAYOUT_ONLY) {
    try {
      for (const viewport of [{ width: 1440, height: 900 }, { width: 1239, height: 843 }, { width: 927, height: 873 }, { width: 390, height: 844 }]) {
        for (const theme of ['light', 'dark']) {
          const context = await browser.newContext({ viewport, reducedMotion: 'reduce', colorScheme: theme });
          const { cookie } = sessions.issue({ sub: 'sub', email: 'owner@example.test' });
          await context.addCookies([{ name: 'dc2_session', value: cookie.split(';')[0].split('=')[1], domain: '.' + BASE, path: '/' }]);
          const page = await context.newPage();
          page.setDefaultTimeout(8000);
          try { await (process.env.CONSOLE_VERIFY_PERFORMANCE_ONLY ? verifyPerformance : process.env.CONSOLE_VERIFY_EVIDENCE_LAYOUT_ONLY ? verifyEvidenceLayout : process.env.CONSOLE_VERIFY_ANNOTATIONS_ONLY ? verifyAnnotations : process.env.CONSOLE_VERIFY_WORKSPACE_ONLY ? verifyWorkspace : process.env.CONSOLE_VERIFY_ARTIFACTS_ONLY ? verifyTestArtifacts : verifyTestsDesign)({ page, daemon, check, scenario: SCENARIOS.populated, baseUrl: `http://${HOST}:${port}/`, output: OUT, theme, viewport }); }
          catch (error) { check(`Tests design ${theme} ${viewport.width}`, false, error.message); }
          await context.close();
        }
      }
      await fs.writeFile(path.join(OUT, 'report.json'), JSON.stringify(report, null, 2));
      console.log(JSON.stringify({ checks: report.checks.length, failures: report.failures, report: path.join(OUT, 'report.json') }));
      process.exitCode = report.failures.length ? 1 : 0;
    } finally { await browser.close(); await edge.close(); await daemon.close(); }
    return;
  }

  if (process.env.CONSOLE_VERIFY_CACHE_ONLY) {
    try {
      for (const viewport of [{ width: 1110, height: 876 }, { width: 390, height: 844 }]) {
        const context = await browser.newContext({ viewport });
        const { cookie } = sessions.issue({ sub: 'sub', email: 'owner@example.test' });
        await context.addCookies([{ name: 'dc2_session', value: cookie.split(';')[0].split('=')[1], domain: '.' + BASE, path: '/' }]);
        const page = await context.newPage();
        for (const route of ['usage', 'usage/' + REPO, 'progress/' + REPO, 'deployments'].filter(route => !process.env.CONSOLE_VERIFY_CACHE_ROUTE || route.startsWith(process.env.CONSOLE_VERIFY_CACHE_ROUTE))) {
          for (const cacheState of ['loading', 'stale', 'failed', 'unavailable']) {
            try {
            daemon.setScenario({ ...SCENARIOS.populated, cacheState });
            const operation = route === 'usage' ? 'usage.repositories' : route.startsWith('progress') ? 'progress.repository' : 'usage.repository';
            const failed = ['failed', 'unavailable'].includes(cacheState);
            const waitRequest = failed ? null : page.waitForRequest(request => request.url().endsWith('/api/v2/' + operation) && request.postDataJSON()?.wait_for_refresh === true).catch(error => ({ error }));
            await page.goto('http://' + HOST + ':' + port + '/?cache=' + cacheState + '#/' + route);
            await page.waitForFunction(() => /Loading usage|Saved usage|Refresh failed|refresh failed/i.test(document.querySelector('main').innerText));
            const before = await page.locator('main').innerText();
            check('cache ' + viewport.width + ' ' + route + ' ' + cacheState + ': truthful snapshot label', cacheState === 'loading' ? /Loading usage/.test(before) : failed ? /refresh failed/i.test(before) : /saved usage/i.test(before));
            if (cacheState === 'unavailable' && route === 'usage/' + REPO) check('empty usage failure is shown once', (before.match(/refresh failed/gi) || []).length === 1 && await page.locator('.usage-metrics').count() === 0);
            if (cacheState === 'loading' && route === 'usage/' + REPO) check('cache cold detail hides unmeasured metrics', await page.locator('.usage-metrics').count() === 0);
            if (cacheState === 'loading' && route === 'progress/' + REPO) check('cache cold Progress leaves token evidence blank', await page.locator('[data-progress-evidence="tokens"] > strong').textContent() === '—');
            const details = route === 'progress/' + REPO ? '.progress-exact' : route === 'usage/' + REPO ? '.usage-provenance' : null;
            if (cacheState === 'stale' && details) await page.locator(details + ' summary').click();
            if (waitRequest) {
              const received = await waitRequest;
              if (received.error) throw received.error;
              daemon.releaseDelayed();
              await page.waitForFunction(() => !/Loading usage|Saved usage · updating|Updating saved usage/.test(document.querySelector('main').innerText));
              if (cacheState === 'stale' && details) check('cache ' + route + ': open details survive refresh', await page.locator(details).getAttribute('open') !== null);
              check('cache ' + route + ': completion uses bounded event wait', daemon.calls.some(call => call.operation === operation && call.params.wait_for_refresh === true));
            }
            check('cache ' + viewport.width + ' ' + route + ': no document overflow', await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth));
            await page.screenshot({ path: path.join(OUT, viewport.width + '-' + route.replaceAll('/', '-') + '-' + cacheState + '-initial.png') });
            await page.screenshot({ path: path.join(OUT, viewport.width + '-' + route.replaceAll('/', '-') + '-' + cacheState + '-full.png'), fullPage: true });
            } catch (error) {
              check('cache ' + viewport.width + ' ' + route + ' ' + cacheState, false, error.message + ' usage cards: ' + (await page.locator('[data-summary="usage"]').allTextContents()).join(' | ').slice(0, 600));
              daemon.releaseDelayed();
            }
          }
        }
        await context.close();
      }
      await fs.writeFile(path.join(OUT, 'report.json'), JSON.stringify(report, null, 2));
      console.log(JSON.stringify({ checks: report.checks.length, failures: report.failures, report: path.join(OUT, 'report.json') }));
      process.exitCode = report.failures.length ? 1 : 0;
    } finally { daemon.releaseDelayed(); await browser.close(); await edge.close(); await daemon.close(); }
    return;
  }

  if (!process.env.CONSOLE_VERIFY_INTERACTIONS_ONLY) for (const [scenarioName, scenario] of Object.entries(SCENARIOS).filter(([, scenario]) => !scenario.targetedOnly)) {
    daemon.setScenario(scenario);
    for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
      const context = await browser.newContext({ viewport, ignoreHTTPSErrors: true });
      const { cookie } = sessions.issue({ sub: 'sub', email: scenario.identity, name: 'Verifier' });
      const [nameValue] = cookie.split(';');
      await context.addCookies([{ name: 'dc2_session', value: nameValue.split('=')[1], domain: `.${BASE}`, path: '/' }]);
      const page = await context.newPage();
      page.on('dialog', (d) => d.accept());
      for (const view of VIEWS) {
        const label = `${scenarioName}-${view.replace(/[#/]+/g, '_').replace(/^_/, '')}-${vpName}`;
        const callsBeforeNavigation = daemon.calls.length;
        const scopedView = ['#/deployments', '#/tests'].includes(view) ? `${view}?repository=${REPO}` : ['#/plan', '#/progress', '#/usage', '#/decisions'].includes(view) ? `${view}/${REPO}` : view;
        await page.goto(`http://${HOST}:${port}/${scopedView}`);
        if (scenario.delayMs) {
          const firstPending = await daemon.waitForReceivedAfter(callsBeforeNavigation);
          if (firstPending.operation === 'user.whoami') {
            daemon.releaseDelayed();
            await daemon.waitForReceivedAfter(callsBeforeNavigation + 1);
          }
          await waitForRenderFrame(page);
          const loading = await page.evaluate(() => ({
            visible: Boolean(document.querySelector('.skeleton')) || /Loading/.test(document.body.innerText),
            hash: location.hash,
            text: document.body.innerText.slice(0, 400),
            html: document.querySelector('main')?.innerHTML.slice(0, 500),
          }));
          if (!loading.visible) {
            loading.calls = daemon.calls.slice(callsBeforeNavigation).map((call) => call.operation);
            throw new Error(`${label}: loading surface did not render: ${JSON.stringify(loading)}`);
          }
        }
        else {
          await page.waitForFunction(() => !document.querySelector('.skeleton'), null, { timeout: 15000 }).catch(() => {});
          await waitForRenderFrame(page);
        }
        await page.screenshot({ path: path.join(OUT, `${label}.png`), fullPage: true });
        const metrics = await page.evaluate((expectedDestinationHref) => {
          const doc = document.documentElement;
          const overflow = doc.scrollWidth - window.innerWidth;
          const clipped = [...document.querySelectorAll('.tile .v, .health-capacity-value strong, .health-status-item > span, .health-storage-breakdown dt, .health-storage-breakdown dd, h1, .page-heading > strong, .toast')].filter((el) => el.scrollWidth > el.clientWidth + 1).map((el) => el.textContent.slice(0, 40));
          const buttons = [...document.querySelectorAll('button')].map((b) => ({ text: b.textContent.trim(), visible: b.offsetParent !== null, disabled: b.disabled, x: b.getBoundingClientRect().right, scrollable: !!b.closest('.tablewrap, .gantt-viewport, .usage-chart-scroll, .evidence-toolbar, .evidence-variants, #evidence-step-list') }));
          const offscreen = buttons.filter((b) => b.visible && b.x > window.innerWidth + 1 && !b.scrollable);
          const planControls = document.querySelectorAll('[data-move-task], [data-drag-task], #comment-form, [data-cmd="release.request"]').length;
          const elaborationControls = document.querySelectorAll('[data-elaborate-task]').length;
          const headingLink = document.querySelector('main h1 a.destination-link');
          const header = document.querySelector('header.top');
          const visibleHeaderChildren = [...header.children].filter((el) => el.offsetParent !== null);
          const centers = visibleHeaderChildren.map((el) => {
            const rect = el.getBoundingClientRect();
            return rect.top + rect.height / 2;
          });
          const headerRect = header.getBoundingClientRect();
          const healthSummary = document.querySelector('.health-summary');
          const healthStorageItems = [...document.querySelectorAll('.health-storage-breakdown > div')];
          const healthCapacityRects = [...document.querySelectorAll('.health-capacity-card')].map((element) => element.getBoundingClientRect());
          const healthCapacityRows = new Map();
          for (const rect of healthCapacityRects) {
            const top = Math.round(rect.top);
            if (!healthCapacityRows.has(top)) healthCapacityRows.set(top, []);
            healthCapacityRows.get(top).push(rect.height);
          }
          const healthTable = document.querySelector('.health-repository-table');
          const healthTableWrap = healthTable?.closest('.tablewrap');
          const healthIncidentHeading = document.querySelector('#health-incidents-title');
          const healthSummaryRect = healthSummary?.getBoundingClientRect();
          const healthStatusRect = document.querySelector('.health-status-panel')?.getBoundingClientRect();
          return {
            overflow, clipped, buttons: buttons.length, offscreen: offscreen.length,
            text: document.body.innerText.slice(0, 4000), skeleton: !!document.querySelector('.skeleton'),
            notice: document.querySelector('.notice')?.textContent || '', planControls,
            elaborationControls,
            headingHref: headingLink?.getAttribute('href') || '',
            expectedDestinationHref,
            headerHeight: headerRect.height,
            headerCenterSpread: centers.length ? Math.max(...centers) - Math.min(...centers) : 0,
            navToggleVisible: document.querySelector('#nav-toggle')?.offsetParent !== null,
            navVisible: document.querySelector('#nav')?.offsetParent !== null,
            projectPickerCount: document.querySelectorAll('[data-project-picker]').length,
            projectNativeSelectCount: document.querySelectorAll('[data-project-picker] select').length,
            sharedRepositoryCount: document.querySelectorAll('#repository-list').length,
            health: healthSummary ? {
              capacityCards: healthCapacityRects.length,
              statusItems: document.querySelectorAll('.health-status-item').length,
              capacityRowHeightSpreads: [...healthCapacityRows.values()].map((heights) => Math.max(...heights) - Math.min(...heights)),
              primaryInInitialViewport: healthSummaryRect.top >= 0 && healthStatusRect.bottom <= window.innerHeight,
              primaryBeforeIncidents: healthSummaryRect.bottom <= healthIncidentHeading.getBoundingClientRect().top + 1,
              storageItems: healthStorageItems.map((element) => ({
                text: element.innerText,
                clipped: element.scrollWidth > element.clientWidth + 1 || element.scrollHeight > element.clientHeight + 1,
                left: element.getBoundingClientRect().left,
                right: element.getBoundingClientRect().right,
              })),
              tableDisplay: healthTable ? getComputedStyle(healthTable).display : '',
              tableScroll: healthTableWrap ? healthTableWrap.scrollWidth - healthTableWrap.clientWidth : 0,
            } : null,
          };
        }, destinationHref(view));
        check(`${label}: no horizontal document overflow`, metrics.overflow <= 0, `overflow ${metrics.overflow}px`);
        check(`${label}: no clipped headline text`, metrics.clipped.length === 0, metrics.clipped.join(' | '));
        check(`${label}: no off-canvas controls outside scroll containers`, metrics.offscreen === 0, `${metrics.offscreen} off-canvas`);
        check(`${label}: destination remains reachable in context`, metrics.headingHref === metrics.expectedDestinationHref || view === '#/tests' && await page.locator('#workspace-aspects a[aria-current][href^="#/tests?repository="]').count() === 1, `${metrics.headingHref} != ${metrics.expectedDestinationHref}`);
        check(`${label}: global header stays on one row`, metrics.headerHeight <= 64 && metrics.headerCenterSpread <= 2, `height ${metrics.headerHeight}px, center spread ${metrics.headerCenterSpread}px`);
        check(`${label}: Console tools use one compact menu`, !metrics.navVisible && metrics.navToggleVisible);
        if (PROJECT_DETAIL_VIEWS.has(view) && ['populated', 'empty', 'applying'].includes(scenarioName)) {
          check(`${label}: one shared repository selector replaces per-page pickers`, metrics.projectPickerCount === 0 && metrics.sharedRepositoryCount === 1);
        }
        if (scenarioName === 'loading') check(`${label}: loading state visible`, metrics.skeleton || /Loading/.test(metrics.text));
        if (scenarioName === 'empty' && !view.includes(DEP) && view !== '#/admin') check(`${label}: explicit empty state`, /No (deployments|test runs|visual evidence|open bugs|containers|repositories|plan|decisions|provider-reported|open work|performance reviews|measured outcomes)/.test(metrics.text), metrics.text.slice(0, 120));
        if (scenarioName === 'error') check(`${label}: error state with retry`, /Could not load|Cannot reach/.test(metrics.text) && /Retry/.test(metrics.text), metrics.text.slice(0, 120));
        if (scenarioName === 'denied' && (view === '#/admin' || view === '#/tests' || view === '#/health/containers')) check(`${label}: permission denied shown`, /Permission denied/.test(metrics.notice), metrics.notice.slice(0, 120));
        if (scenarioName === 'denied' && view.startsWith('#/usage')) check(`${label}: usage requires operator access`, /Permission denied/.test(metrics.notice), metrics.notice.slice(0, 120));
        if (scenarioName === 'denied' && view.startsWith('#/progress')) check(`${label}: progress requires operator access`, /Permission denied/.test(metrics.notice), metrics.notice.slice(0, 120));
        if (scenarioName === 'denied' && view === '#/health') check(`${label}: host health denied but repositories visible`, /administrator-only/.test(metrics.text) && /repo-one/.test(metrics.text));
        if (scenarioName === 'denied' && view === '#/deployments') {
          check(`${label}: viewers retain read-only deployment facts`, await page.locator('.deployment-record').count() === 2 && await page.locator('[data-edit-domain], [data-cmd]:not(:disabled)').count() === 0);
        }
        if (scenarioName === 'applying' && (view === '#/deployments' || view === `#/deployments/${DEP}`)) {
          const surface = view === '#/deployments'
            ? page.locator(`a[href="#/deployments/${DEP}"]`).locator('xpath=ancestor::article[contains(@class,"deployment-record")]')
            : page.locator('main');
          const enabledMutation = await surface.locator('[data-cmd^="deployment."]:not(:disabled)').count();
          check(`${label}: applying deployment has no enabled conflicting mutation`, enabledMutation === 0, `${enabledMutation} enabled`);
          if (view === `#/deployments/${DEP}`) check(`${label}: applying journey explains lost replies`, /Closing this page does not cancel/.test(metrics.text));
        }
        if (scenarioName === 'populated' && ['#/deployments', '#/tests'].includes(view)) check(`${label}: long repository names remain selectable`, /going-and-going/.test(await page.locator('#repository-list').textContent()));
        if (scenarioName === 'populated' && view === '#/deployments') {
          check(`${label}: deployments stay within the selected repository`,
            await page.locator('.deployment-record').count() === 2
            && await page.locator(`[data-deployment-id="${DEP}"]`).count() === 1
            && await page.locator(`[data-deployment-id="${OBS}"]`).count() === 0
            && await page.locator('.deployment-repository-summary').count() === 0);
        }
        if (scenarioName === 'populated' && ['#/tests', '#/health'].includes(view)) {
          const numberText = view === '#/tests' ? await page.locator('.test-technical').first().textContent() : metrics.text;
          check(`${label}: large numbers humanized`, /MiB|GiB|TiB/.test(numberText), numberText.slice(0, 80));
        }
        if (scenarioName === 'populated' && view === '#/health') {
          check(`${label}: host capacity and operational status lead as one aligned summary`,
            metrics.health?.capacityCards === 4
            && metrics.health?.statusItems === 4
            && metrics.health?.capacityRowHeightSpreads.every((spread) => spread <= 1)
            && metrics.health?.primaryInInitialViewport
            && metrics.health?.primaryBeforeIncidents,
            JSON.stringify(metrics.health));
          check(`${label}: every shared storage category is visible and contained`,
            metrics.health?.storageItems.length === 5
            && metrics.health.storageItems.every((item) => !item.clipped && item.left >= -1 && item.right <= viewport.width + 1)
            && ['Docker shared', 'Docker images', 'Docker build cache', 'Docker shared volumes', 'Other'].every((name) => metrics.health.storageItems.some((item) => item.text.includes(name))),
            JSON.stringify(metrics.health?.storageItems));
          check(`${label}: repository attribution does not need horizontal scrolling`, metrics.health?.tableScroll <= 1, `scroll ${metrics.health?.tableScroll}px`);
        }
        if (scenarioName === 'populated' && view === `#/plan/${REPO}`) {
          check(`${label}: plan shows owner controls for the administrator`, metrics.planControls >= 3, `${metrics.planControls} controls`);
          check(`${label}: every visible task offers an elaboration request to the administrator`, metrics.elaborationControls >= 10, `${metrics.elaborationControls} controls`);
          check(`${label}: plan speaks plainly (no agent-facing technical note)`, !metrics.text.includes('tech-note-marker-must-not-render'));
          check(`${label}: sizes are in lines, statuses in plain words`, /lines/.test(metrics.text) && /being built/.test(metrics.text), metrics.text.slice(0, 120));
        }
        if (scenarioName === 'denied' && view === `#/plan/${REPO}`) check(`${label}: read-only plan for viewers (no owner controls)`, metrics.planControls === 0 && metrics.elaborationControls === 0, `${metrics.planControls} plan controls, ${metrics.elaborationControls} elaboration controls`);
        if (scenarioName === 'populated' && view === `#/usage/${REPO}`) {
          check(`${label}: repository and primary usage trend lead the page`, /Codex Usage/.test(metrics.text) && /repo-one/.test(metrics.text) && await page.locator('[data-ui-region="usage-primary-trend"] svg').count() === 1);
          check(`${label}: usage never exposes collector or captured-content identities`, !/agent-private|request-private|\/home\//.test(metrics.text));
          check(`${label}: usage provides exact chart values`, await page.locator('.usage-exact table').count() === 1);
        }
        if (scenarioName === 'populated' && view === `#/progress/${REPO}`) {
          check(`${label}: progress leads with the release forecast and daily progress`,
            /Likely release:/.test(metrics.text) && /Daily progress/.test(metrics.text)
            && await page.locator('[data-ui-region="progress-forecast"]').count() === 1
            && await page.locator('.progress-pulse-chart').count() === 2
            && await page.locator('[data-ui-region="progress-release-work"]').count() === 1);
          check(`${label}: progress shows factual Plan-ordered work without heuristic claims`,
            /Work in this release/.test(metrics.text)
            && /Shown in Plan order/.test(metrics.text)
            && !/Priority queue|Release impact|dependency|ranked by|\d+\.\d+ days|TECHNICAL-(?:UNBLOCK|REOPEN)-MARKER/.test(metrics.text));
          check(`${label}: progress distinguishes daily bars from running-total lines`,
            await page.locator('.progress-completed-bar').count() > 0
            && await page.locator('.progress-incoming-bar').count() > 0
            && await page.locator('.progress-running-line').count() === 2
            && /Solid above = completed · outlined below = incoming/.test(metrics.text)
            && /Line = completed running total/.test(metrics.text));
          const opposingGeometry = await page.locator('.progress-bar-line-chart').evaluateAll((charts) => charts.every((chart) => {
            const baseline = Number(chart.querySelector('.progress-zero-line')?.getAttribute('y1'));
            const completed = [...chart.querySelectorAll('.progress-completed-bar')];
            const incoming = [...chart.querySelectorAll('.progress-incoming-bar')];
            return Number.isFinite(baseline) && completed.length > 0 && incoming.length > 0
              && completed.every((bar) => Number(bar.getAttribute('y')) < baseline
                && Number(bar.getAttribute('y')) + Number(bar.getAttribute('height')) <= baseline + .1)
              && incoming.every((bar) => Number(bar.getAttribute('y')) >= baseline
                && Number(bar.getAttribute('height')) > 0);
          }));
          check(`${label}: completed work is above the baseline and added work is below it`, opposingGeometry);
          const labelLayering = await page.locator('.progress-bar-line-chart').evaluateAll((charts) => charts.every((chart) => {
            const line = chart.querySelector('.progress-running-line');
            const labels = [...chart.querySelectorAll('.progress-bar-value')];
            return line && labels.length > 0
              && labels.every((value) => Boolean(line.compareDocumentPosition(value) & Node.DOCUMENT_POSITION_FOLLOWING))
              && labels.every((value) => getComputedStyle(value).paintOrder.startsWith('stroke'));
          }));
          check(`${label}: progress value labels paint above the running line with a readability halo`, labelLayering);
          check(`${label}: progress labels estimated lines truthfully`,
            /Planned lines completed and added/.test(metrics.text)
            && /Completed above · added below/.test(metrics.text)
            && !/Git lines completed/.test(metrics.text));
        }
        if (scenario.delayMs) {
          await page.goto('about:blank');
          daemon.releaseDelayed();
        }
      }
      await context.close();
    }
  }

  if (process.env.CONSOLE_VERIFY_STATES_ONLY) {
    await browser.close(); await edge.close(); await daemon.close();
    await fs.writeFile(path.join(OUT, 'report.json'), JSON.stringify(report, null, 2));
    console.log(JSON.stringify({ checks: report.checks.length, failures: report.failures }));
    process.exitCode = report.failures.length ? 1 : 0;
    return;
  }

  // Interaction proofs (populated, wide): controls call the API and the view re-renders.
  daemon.setScenario(SCENARIOS.populated);
  const context = await browser.newContext({ viewport: VIEWPORTS.wide, hasTouch: true });
  const { cookie } = sessions.issue({ sub: 'sub', email: 'owner@example.test' });
  await context.addCookies([{ name: 'dc2_session', value: cookie.split(';')[0].split('=')[1], domain: `.${BASE}`, path: '/' }]);
  const page = await context.newPage();
  let nativeDialogCount = 0;
  page.on('dialog', (dialog) => { nativeDialogCount += 1; dialog.accept(); });
  const pointerDrag = async (sourceSelector, targetSelector, targetPosition = {}) => {
    const source = page.locator(sourceSelector);
    const target = page.locator(targetSelector);
    await source.scrollIntoViewIfNeeded();
    const sourceBox = await source.boundingBox();
    const targetBox = await target.boundingBox();
    const start = { x: sourceBox.x + sourceBox.width / 2,
      y: sourceBox.y + sourceBox.height / 2 };
    const finish = { x: targetBox.x + (targetPosition.x ?? targetBox.width / 2),
      y: targetBox.y + (targetPosition.y ?? targetBox.height / 2) };
    await page.mouse.move(start.x, start.y);
    await page.mouse.down();
    await page.mouse.move(start.x + 4, start.y + 2, { steps: 2 });
    await page.mouse.move(finish.x, finish.y, { steps: 8 });
    await page.mouse.up();
  };
  await page.goto(`http://${HOST}:${port}/#/deployments/${DEP}`);
  await page.waitForSelector('button[data-cmd="deployment.stop"]');
  await page.click('h1 ~ .actions button[data-cmd="deployment.stop"]');
  await page.waitForFunction(() => [...document.querySelectorAll('h1 .badge')].some((b) => b.textContent === 'stopped'), null, { timeout: 10000 });
  check('interaction: stop calls deployment.stop and the header shows stopped', daemon.calls.some((c) => c.operation === 'deployment.stop' && c.params.deployment_id === DEP && c.client.identity === 'owner@example.test'));
  await page.click('h1 ~ .actions button[data-cmd="deployment.start"]');
  await page.waitForFunction(() => [...document.querySelectorAll('h1 .badge')].some((b) => b.textContent === 'running'), null, { timeout: 10000 });
  check('interaction: start restores running', true);
  daemon.calls.length = 0;
  const projectionRow = page.locator('[data-compose-service="stack/projection-worker"]');
  await projectionRow.locator('[data-cmd="deployment.stop"]').click();
  await page.waitForFunction(() => [...document.querySelectorAll('h1 .badge')].some((b) => b.textContent === 'degraded'), null, { timeout: 10000 });
  check('interaction: independent Compose stop targets only the reviewed service',
    daemon.calls.some((c) => c.operation === 'deployment.stop' && c.params.component === 'stack/projection-worker'));
  check('interaction: stopped Compose service remains visible and the route stays published',
    /stopped/.test(await projectionRow.innerText()) && /20002/.test(await page.innerText('main')));
  await projectionRow.locator('[data-cmd="deployment.start"]').click();
  await page.waitForFunction(() => [...document.querySelectorAll('h1 .badge')].some((b) => b.textContent === 'running'), null, { timeout: 10000 });
  check('interaction: independent Compose start restores running without a stack apply',
    daemon.calls.some((c) => c.operation === 'deployment.start' && c.params.component === 'stack/projection-worker')
    && !daemon.calls.some((c) => c.operation === 'deployment.apply'));
  await page.click('button[data-logs="api"]');
  await page.waitForSelector('pre.log');
  check('interaction: logs load on demand', daemon.calls.some((c) => c.operation === 'deployment.logs' && c.params.component === 'api'));
  await page.getByRole('button', { name: 'Remove deployment — keep data' }).click();
  await waitForSettledCall(daemon, page, 'deployment.remove');
  const keepDataCall = daemon.calls.find((c) => c.operation === 'deployment.remove');
  check('interaction: keep-data removal is an explicit immediate action',
    keepDataCall && keepDataCall.params.deployment_id === DEP && keepDataCall.params.delete_data === false);
  await page.goto(`http://${HOST}:${port}/#/deployments/${DEP}`);
  await page.waitForSelector('button[data-cmd="deployment.remove"]');
  daemon.calls.length = 0;
  await page.getByRole('button', { name: 'Remove deployment and delete data' }).click();
  await waitForSettledCall(daemon, page, 'deployment.remove');
  const deleteDataCall = daemon.calls.find((c) => c.operation === 'deployment.remove');
  check('interaction: delete-data removal is a separate explicit immediate action',
    deleteDataCall && deleteDataCall.params.deployment_id === DEP && deleteDataCall.params.delete_data === true);
  // Domain editing (DC2-2026-08-24: administrators edit the routed domain in place).
  await page.goto(`http://${HOST}:${port}/#/deployments/${DEP}`);
  await page.waitForSelector('#edit-domain');
  await page.click('#edit-domain');
  await page.waitForSelector('dialog#domain-dialog[open]');
  await page.fill('#domain-form [name=domain]', 'renamed-app');
  await page.click('#domain-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'deployment.set_domain');
  const domainCall = daemon.calls.find((c) => c.operation === 'deployment.set_domain');
  check('interaction: domain pop-up calls deployment.set_domain with the new label',
    domainCall && domainCall.params.deployment_id === DEP && domainCall.params.domain === 'renamed-app');
  await page.goto(`http://${HOST}:${port}/#/deployments?repository=${REPO}`);
  await page.waitForSelector('.deployment-record');
  check('deployments: the selected repository shows every deployment without duplicate aspect summaries',
    await page.locator('.deployment-record').count() === 2 && await page.locator('.deployment-repository-summary').count() === 0);
  await chooseRepository(page, 'legacy-repo');
  await page.waitForSelector(`[data-deployment-id="${OBS}"]`);
  check('deployments: switching repositories never mixes another repository’s deployments',
    await page.locator('.deployment-record').count() === 1 && await page.locator(`[data-deployment-id="${DEP}"]`).count() === 0);
  daemon.calls.length = 0;
  await page.click(`[data-edit-domain="${OBS}"]`);
  await page.waitForSelector('dialog#domain-dialog[open]');
  await page.fill('#domain-form [name=domain]', 'from-list');
  await page.click('#domain-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'deployment.set_domain');
  const listDomainCall = daemon.calls.find((c) => c.operation === 'deployment.set_domain');
  check('interaction: list-row domain edit opens the pop-up and edits that deployment',
    listDomainCall && listDomainCall.params.deployment_id === OBS && listDomainCall.params.domain === 'from-list');
  await page.goto(`http://${HOST}:${port}/#/deployments`);
  const observedRow = page.locator(`a[href="#/deployments/${OBS}"]`).locator('xpath=ancestor::article[contains(@class,"deployment-record")]');
  await observedRow.waitFor();
  check('observed deployment list row offers start/stop/restart',
    await observedRow.locator('[data-cmd="deployment.restart"]').count() === 1);
  check('observed deployment list row offers no apply',
    await observedRow.locator('[data-cmd="deployment.apply"]').count() === 0);
  check('observed deployment list row is explicitly labelled', /observed/i.test(await observedRow.innerText()));
  await page.goto(`http://${HOST}:${port}/#/deployments/${OBS}`);
  await page.waitForSelector('text=exact recorded containers');
  check('observed deployment detail offers lifecycle and log controls but no configuration authority',
    await page.locator('[data-cmd="deployment.restart"]').count() >= 1
    && await page.locator('[data-logs]').count() >= 1
    && await page.locator('[data-cmd="deployment.apply"], [data-cmd="deployment.rollback"], [data-cmd="deployment.remove"]').count() === 0);
  await page.click('h1 ~ .actions button[data-cmd="deployment.restart"]');
  await waitForSettledCall(daemon, page, 'deployment.restart');
  check('interaction: observed restart calls deployment.restart on the observed id',
    daemon.calls.some((c) => c.operation === 'deployment.restart' && c.params.deployment_id === OBS));
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await revealTestRows(page);
  const runningRow = page.locator('[data-test-run-id="t20260101T000000Z-abc123"]');
  const visualRow = page.locator(`[data-test-run-id="${TEST_RUN}"]`);
  await chooseRepository(page, LONG);
  await visualRow.locator('.test-thumbnail').first().waitFor();
  check('tests: evidence availability is truthful before opening a run',
    await visualRow.locator('.test-thumbnail').count() === 4
    && await visualRow.locator('.test-preview-more').count() === 1);
  await chooseRepository(page, 'repo-one');
  check('tests: nonvisual runs do not claim to have screenshots', await runningRow.locator('.test-thumbnail, a[href^="#/tests/"]').count() === 0);
  await page.setViewportSize(VIEWPORTS.narrow);
  const narrowTests = await page.evaluate(() => {
    const collection = document.querySelector('.test-results');
    const evidence = document.querySelector('[data-test-run-id="t20260101T000000Z-abc123"] .test-result-summary>.badge');
    const rect = evidence?.getBoundingClientRect();
    return {
      documentOverflow: document.documentElement.scrollWidth - innerWidth,
      collectionOverflow: collection ? collection.scrollWidth - collection.clientWidth : null,
      evidenceRect: rect ? { left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom } : null,
    };
  });
  check('tests: narrow rows expose evidence and controls without a horizontal discovery path',
    narrowTests.documentOverflow <= 0
    && narrowTests.collectionOverflow <= 0
    && narrowTests.evidenceRect?.left >= 0
    && narrowTests.evidenceRect?.right <= VIEWPORTS.narrow.width
    && narrowTests.evidenceRect?.bottom <= VIEWPORTS.narrow.height,
  JSON.stringify(narrowTests));
  await page.setViewportSize(VIEWPORTS.wide);
  await revealTestSettings(page); await page.click('#test-capacity-open');
  await page.waitForSelector('dialog#test-capacity-dialog[open]');
  await page.locator('#test-capacity-dialog input').focus();
  daemon.setScenario({ ...SCENARIOS.populated, testFinished: true, targetedOnly: true });
  await page.waitForFunction(() => {
    const row = document.querySelector('[data-test-run-id="t20260101T000000Z-abc123"]');
    return row && /passed/.test(row.textContent) && !row.querySelector('.test-thumbnail');
  });
  check('tests: live refresh replaces stale running state and stop action without closing active work',
    await page.locator('dialog#test-capacity-dialog[open]').count() === 1
    && await page.locator('#test-capacity-dialog:focus-within').count() === 1
    && await runningRow.getByRole('button', { name: 'Stop run', exact: true }).count() === 0
    && await page.innerText('#test-live-status') === '');
  await page.click('#test-capacity-cancel');
  daemon.setScenario(SCENARIOS.populated);
  await page.goto(`http://${HOST}:${port}/#/health`);
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await page.waitForSelector('#test-live-status', { state: 'attached' });
  daemon.setScenario(SCENARIOS.error);
  await page.waitForFunction(() => /Updates paused/.test(document.querySelector('#test-live-status')?.textContent || ''));
  daemon.setScenario({ ...SCENARIOS.populated, testFinished: true, targetedOnly: true });
  await page.waitForFunction(() => /passed/.test(document.querySelector('[data-test-run-id="t20260101T000000Z-abc123"]')?.textContent || '') && document.querySelector('#test-live-status')?.textContent === '');
  check('tests: a failed live read preserves the page and recovers on the next bounded refresh',
    /passed/i.test(await page.innerText('[data-test-run-id="t20260101T000000Z-abc123"]'))
    && await page.locator('.test-results').count() === 1);
  daemon.setScenario(SCENARIOS.populated);
  await page.goto(`http://${HOST}:${port}/#/health`);
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await revealTestRows(page);
  daemon.calls.length = 0;
  await revealTestRows(page); await page.locator('button[data-test-logs]').first().click();
  await page.waitForSelector('dialog#test-logs-dialog[open] #test-log-read-result pre.log');
  const catalogIndex = daemon.calls.findIndex((c) => c.operation === 'test.log.catalog');
  const initialTailIndex = daemon.calls.findIndex((c) => c.operation === 'test.log.tail');
  check('interaction: Logs catalogues first and then opens readable output automatically',
    catalogIndex >= 0 && initialTailIndex > catalogIndex);
  check('tests: stream names describe human-readable output instead of repeating internal phases',
    await page.locator('#test-log-stream option').allTextContents().then((options) => options.join(' | ') === 'unit · parser-17 · Error output | Test runner · Standard output | structured · Standard output'));
  check('tests: ordinary log reading exposes no line, byte, start, end, or numeric range form',
    await page.locator('#test-logs-dialog input[type="number"], #test-log-range').count() === 0
    && !/Range type|Read range/.test(await page.innerText('#test-logs-dialog')));
  check('tests: concise stream status stays visible while technical metadata is collapsed',
    /42,?000 lines · 8(?:\.0)? MiB · Complete · Retained in /.test(await page.innerText('#test-log-summary'))
    && !(await page.locator('.test-log-details').evaluate((element) => element.open)));
  await page.click('.test-log-details summary');
  const logMetadata = await page.innerText('#test-log-metadata');
  check('tests: log catalogue exposes counts, completion, hash, expiry, and structured evidence without an absolute path',
    /8(?:\.0)? MiB/.test(logMetadata) && /42,?000/.test(logMetadata) && /Complete\s+Yes/.test(logMetadata)
    && /Truncated\s+No/.test(logMetadata) && /junit/.test(logMetadata)
    && new RegExp(`a{64}`).test(logMetadata) && /Expires\s+in /.test(logMetadata)
    && /History depth\s+1 of 3/.test(logMetadata)
    && !(await page.innerText('#test-logs-dialog')).includes('/srv/repos/'), logMetadata);
  await page.click('.test-log-details summary');
  check('interaction: automatic output uses the exact catalogued check, case, phase, and stream', daemon.calls.some((c) => c.operation === 'test.log.tail'
    && c.params.check === 'unit' && c.params.phase === 'case' && c.params.case === 'parser-17'
    && c.params.stream === 'stderr' && c.params.lines === 200 && c.params.max_bytes === 49152));
  check('tests: retrieved output carries stable source-line coordinates', /Lines 41801–42000/.test(await page.innerText('#test-log-read-result')));
  check('tests: every retrieved stream is explicitly labelled untrusted',
    await page.locator('#test-log-read-result pre[aria-label="Untrusted log text"]').count() === 1
    && /Untrusted log output/.test(await page.innerText('.test-log-view-head')));
  check('tests: raw text highlights numbers, timestamps, and textual outcomes without creating source markup',
    await page.locator('#test-log-read-result .log-token-number').count() >= 200
    && await page.locator('#test-log-read-result .log-token-time').count() === 1
    && await page.locator('#test-log-read-result .log-token-warning').count() >= 1
    && await page.locator('#test-log-read-result .log-token-success').count() >= 1
    && await page.locator('#test-log-read-result .log-token-failure').count() >= 1
    && await page.locator('#test-log-read-result script').count() === 0
    && /<script>alert\(1\)<\/script>/.test(await page.innerText('#test-log-read-result')));
  check('tests: no visible or hidden manual paging control remains in the log reader',
    await page.locator('#test-log-page, .log-page-button').count() === 0
    && !/Load earlier output|Show more matches|Show more failures/.test(await page.innerText('#test-logs-dialog')));
  const beforeEarlier = await page.locator('#test-log-read-result').evaluate((element) => {
    element.scrollTop = 0;
    const anchor = [...element.querySelectorAll('.log-result')].find((entry) => entry.textContent.includes('build step 41801'));
    return { top: element.scrollTop, height: element.scrollHeight,
      anchorTop: anchor?.getBoundingClientRect().top - element.getBoundingClientRect().top };
  });
  await page.locator('#test-log-read-result').evaluate((element) => element.dispatchEvent(new Event('scroll')));
  await waitForSettledCall(daemon, page, (call) => call.operation === 'test.log.tail' && call.params.cursor === 'older-tail');
  await page.waitForFunction(() => document.querySelector('#test-log-read-result')?.textContent.includes('earlier setup output 1'));
  const afterEarlier = await page.locator('#test-log-read-result').evaluate((element) => {
    const anchor = [...element.querySelectorAll('.log-result')].find((entry) => entry.textContent.includes('build step 41801'));
    return { top: element.scrollTop, height: element.scrollHeight,
      anchorTop: anchor?.getBoundingClientRect().top - element.getBoundingClientRect().top };
  });
  const progressiveText = await page.innerText('#test-log-read-result');
  check('interaction: reaching the upper boundary follows the exact cursor and prepends without duplicates',
    progressiveText.indexOf('earlier setup output 1') >= 0
    && progressiveText.indexOf('earlier setup output 1') < progressiveText.indexOf('assertion failed')
    && (progressiveText.match(/assertion failed/g) || []).length === 1);
  check('interaction: prepending older output preserves the prior reading position',
    afterEarlier.height > beforeEarlier.height && afterEarlier.top > beforeEarlier.top
    && Math.abs(afterEarlier.anchorTop - beforeEarlier.anchorTop) <= 2,
  JSON.stringify({ beforeEarlier, afterEarlier }));
  await page.locator('#test-log-read-result').evaluate((element) => {
    element.scrollTop = 0; element.dispatchEvent(new Event('scroll'));
  });
  await waitForSettledCall(daemon, page, (call) => call.operation === 'test.log.tail' && call.params.cursor === 'oldest-tail');
  await page.waitForFunction(() => document.querySelector('#test-log-read-result')?.textContent.includes('oldest retained output 1'));
  check('interaction: repeated upward scrolling continues until the retained beginning',
    /Start of output/.test(await page.innerText('#test-log-read-result'))
    && (await page.innerText('#test-log-read-result')).indexOf('oldest retained output 1')
      < (await page.innerText('#test-log-read-result')).indexOf('earlier setup output 1'));
  check('tests: browsing older output offers a plain return to the newest lines',
    await page.locator('#test-log-latest:visible').count() === 1
    && await page.innerText('#test-log-latest') === 'Jump to latest');
  const beforeLatest = daemon.calls.length;
  await page.click('#test-log-latest');
  await waitForSettledCall(daemon, page, (call) => daemon.calls.indexOf(call) >= beforeLatest
    && call.operation === 'test.log.tail' && !call.params.cursor);
  check('interaction: Jump to latest re-reads the newest bounded output',
    !/earlier setup output/.test(await page.innerText('#test-log-read-result'))
    && /assertion failed/.test(await page.innerText('#test-log-read-result')));
  await page.fill('#test-log-search [name=text]', '[literal].*');
  await page.click('#test-log-search button[type=submit]');
  await waitForSettledCall(daemon, page, (call) => call.operation === 'test.log.search' && call.params.cursor === 'next-search');
  await page.waitForFunction(() => document.querySelector('#test-log-read-result')?.textContent.includes('pending state persisted'));
  check('interaction: search remains literal and bounded', daemon.calls.some((c) => c.operation === 'test.log.search'
    && c.params.text === '[literal].*' && c.params.max_matches === 20 && c.params.context_lines === 2));
  check('tests: search changes the reader mode and extends automatically without a paging control',
    /Search results/.test(await page.innerText('.test-log-view-head'))
    && await page.locator('#test-log-page, .log-page-button').count() === 0
    && /All results shown/.test(await page.innerText('#test-log-read-result')));
  check('interaction: search continuation uses the exact returned cursor without replacing prior results',
    /assertion failed/.test(await page.innerText('#test-log-read-result'))
    && /pending state persisted/.test(await page.innerText('#test-log-read-result')));
  await page.click('[data-log-read="failure_context"]');
  await waitForSettledCall(daemon, page, 'test.log.failure_context');
  await page.waitForFunction(() => /Likely failure/.test(
    document.querySelector('.test-log-view-head')?.textContent || ''));
  check('interaction: Show likely failure calls the deterministic Coordinator operation and labels the result plainly', daemon.calls.some((c) => c.operation === 'test.log.failure_context'
    && c.params.limit === 20 && c.params.context_lines === 2)
    && /Likely failure/.test(await page.innerText('.test-log-view-head')));
  const beforeStreamChange = daemon.calls.length;
  await page.selectOption('#test-log-stream', '1');
  await waitForSettledCall(daemon, page, (call) => daemon.calls.indexOf(call) >= beforeStreamChange
    && call.operation === 'test.log.tail' && call.params.phase === 'executor' && call.params.stream === 'stdout');
  check('interaction: choosing another stream opens and pretty-prints its newest structured text automatically',
    /executor finished successfully/.test(await page.innerText('#test-log-read-result'))
    && /Formatted JSON lines/.test(await page.innerText('#test-log-read-result'))
    && await page.locator('#test-log-read-result .log-result.structured').count() === 1
    && await page.locator('#test-log-read-result .log-token-key').count() >= 6
    && await page.locator('#test-log-read-result .log-token-number').count() >= 3
    && await page.locator('#test-log-read-result .log-token-keyword').count() >= 2
    && await page.locator('#test-log-read-result .log-token-string').count() >= 3);
  check('tests: an active stream offers an explicit refresh without exposing coordinates',
    await page.locator('#test-log-latest:visible').count() === 1
    && await page.innerText('#test-log-latest') === 'Refresh latest'
    && /In progress · Active/.test(await page.innerText('#test-log-summary')));
  await page.selectOption('#test-log-stream', '2');
  await page.waitForFunction(() => document.querySelector('#test-log-read-result')?.textContent.includes('Formatted JSON'));
  check('tests: a whole JSON record is pretty-printed, highlighted, and hostile markup stays literal',
    /Formatted JSON/.test(await page.innerText('#test-log-read-result'))
    && /"passed": 12/.test(await page.innerText('#test-log-read-result'))
    && await page.locator('#test-log-read-result .log-token-key').count() >= 6
    && await page.locator('#test-log-read-result .log-token-number').count() >= 3
    && await page.locator('#test-log-read-result img, #test-log-read-result [onerror]').count() === 0
    && /<img src=x onerror=alert\(1\)>/.test(await page.innerText('#test-log-read-result')));
  check('tests: the Console never calls exact numeric range retrieval',
    !daemon.calls.some((c) => c.operation === 'test.log.range'));
  daemon.setScenario({ ...SCENARIOS.populated, testFinished: true, targetedOnly: true });
  await page.waitForFunction(() => /passed/.test(document.querySelector('[data-test-run-id="t20260101T000000Z-abc123"]')?.textContent || ''));
  await page.click('#test-logs-dialog .dialog-close');
  check('interaction: closing Logs returns focus after live refresh replaced the invoking row',
    await page.locator('[data-test-run-id="t20260101T000000Z-abc123"] button[data-test-logs]:focus').count() === 1);
  daemon.setScenario(SCENARIOS.populated);
  check('tests: the run collection remains primary and capacity details stay in the action dialog',
    await page.locator('.workspace-tests-heading h1').count() === 1
    && await page.locator('.test-results').count() === 1
    && await page.locator('#test-capacity-dialog').count() === 0
    && await page.locator('#test-runs-collection > h1, #test-runs-collection > h2').count() === 0);
  await revealTestSettings(page); await page.click('#test-log-retention-open');
  await page.waitForSelector('dialog#test-log-retention-dialog[open]');
  const retentionAge = page.locator('#test-log-retention-form [name=max_age_hours]');
  const retentionDepth = page.locator('#test-log-retention-form [name=case_depth]');
  await retentionAge.fill('2'); await retentionAge.blur();
  await retentionDepth.fill('5'); await retentionDepth.blur();
  check('interaction: retention form accepts both edited boundaries',
    await page.inputValue('#test-log-retention-form [name=max_age_hours]') === '2'
    && await page.inputValue('#test-log-retention-form [name=case_depth]') === '5',
  `${await page.inputValue('#test-log-retention-form [name=max_age_hours]')} / ${await page.inputValue('#test-log-retention-form [name=case_depth]')}`);
  await page.click('#test-log-retention-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'test.log.retention.set');
  check('interaction: retention saves both boundaries directly and schedules cleanup', daemon.calls.some((c) => c.operation === 'test.log.retention.set'
    && c.params.max_age_seconds === 7200 && c.params.case_depth === 5),
  JSON.stringify(daemon.calls.filter((c) => c.operation === 'test.log.retention.set').map((c) => c.params)));
  await page.waitForSelector('#nav-toggle:focus');
  check('interaction: saving retention returns focus to Console tools', await page.locator('#nav-toggle:focus').count() === 1);
  await revealTestSettings(page); await page.click('#test-log-retention-open');
  await page.waitForSelector('dialog#test-log-retention-dialog[open]');
  await page.click('[data-retention-cancel]');
  check('interaction: cancelling retention preserves context and returns focus', await page.locator('#nav-toggle:focus').count() === 1);
  await page.setViewportSize(VIEWPORTS.narrow);
  await revealTestRows(page); await page.locator('button[data-test-logs]').first().click();
  await page.waitForSelector('dialog#test-logs-dialog[open] #test-log-read-result pre.log');
  const narrowLogs = await page.locator('#test-logs-dialog').boundingBox();
  check('tests: Logs remains visible and contained at the narrow viewport', narrowLogs
    && narrowLogs.x >= -1 && narrowLogs.y >= -1
    && narrowLogs.x + narrowLogs.width <= VIEWPORTS.narrow.width + 1
    && narrowLogs.y + Math.min(narrowLogs.height, VIEWPORTS.narrow.height) <= VIEWPORTS.narrow.height + 1,
  JSON.stringify(narrowLogs));
  check('tests: narrow Logs still leads with readable output and no numeric range form',
    /assertion failed/.test(await page.innerText('#test-log-read-result'))
    && await page.locator('#test-logs-dialog input[type="number"], #test-log-range').count() === 0
    && (await page.locator('.test-log-viewer').boundingBox())?.height >= 180);
  const narrowToolbar = await page.locator('.test-log-toolbar').boundingBox();
  const narrowFailure = await page.locator('[data-log-read="failure_context"]').boundingBox();
  check('tests: the lone narrow log action uses the full row instead of leaving a dead half-column',
    narrowToolbar && narrowFailure && narrowFailure.width >= narrowToolbar.width - 1,
  JSON.stringify({ narrowToolbar, narrowFailure }));
  await page.click('#test-logs-dialog .dialog-close');
  daemon.setScenario(SCENARIOS.logEmpty);
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await revealTestRows(page);
  daemon.calls.length = 0;
  await revealTestRows(page); await page.locator('button[data-test-logs]').first().click();
  await page.waitForSelector('#test-log-catalog .notice');
  check('tests: a run with no retained streams shows an honest empty state without attempting a content read',
    /No retained logs for this run/.test(await page.innerText('#test-log-catalog'))
    && !daemon.calls.some((call) => call.operation === 'test.log.tail'));
  await page.click('#test-logs-dialog .dialog-close');
  daemon.setScenario(SCENARIOS.logCatalogError);
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await revealTestRows(page);
  await revealTestRows(page); await page.locator('button[data-test-logs]').first().click();
  await page.waitForSelector('#test-log-catalog-retry');
  check('tests: a failed stream catalogue has one dialog-specific retry',
    /The log catalogue is temporarily unavailable/.test(await page.innerText('#test-log-catalog'))
    && await page.locator('#test-log-catalog button').count() === 1);
  await page.click('#test-log-catalog-retry');
  await page.waitForSelector('#test-log-read-result pre.log');
  check('interaction: catalogue retry recovers and then opens newest output',
    daemon.calls.filter((call) => call.operation === 'test.log.catalog').length === 2
    && daemon.calls.filter((call) => call.operation === 'test.log.tail').length === 1);
  await page.click('#test-logs-dialog .dialog-close');
  daemon.setScenario(SCENARIOS.logCatalogPaged);
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await revealTestRows(page);
  await revealTestRows(page); await page.locator('button[data-test-logs]').first().click();
  await page.waitForSelector('#test-log-more');
  await page.click('#test-log-more');
  await page.waitForFunction(() => document.querySelectorAll('#test-log-stream option').length === 4);
  await page.waitForFunction(() => /assertion failed/.test(
    document.querySelector('#test-log-read-result')?.textContent || ''));
  check('interaction: Show more streams follows the catalogue cursor and preserves the reader',
    daemon.calls.some((call) => call.operation === 'test.log.catalog' && call.params.cursor === 'more-streams')
    && await page.locator('#test-log-more').count() === 0
    && /assertion failed/.test(await page.innerText('#test-log-read-result')));
  await page.click('#test-logs-dialog .dialog-close');
  daemon.setScenario(SCENARIOS.logShortPaged);
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await revealTestRows(page);
  daemon.calls.length = 0;
  await revealTestRows(page); await page.locator('button[data-test-logs]').first().click();
  await waitForSettledCall(daemon, page, (call) => call.operation === 'test.log.tail' && call.params.cursor === 'older-tail');
  await page.waitForFunction(() => document.querySelector('#test-log-read-result')?.textContent.includes('earlier setup output 1'));
  check('interaction: a short newest page auto-fills from one bounded earlier cursor without a paging control',
    daemon.calls.filter((call) => call.operation === 'test.log.tail').length === 2
    && await page.locator('#test-log-page, .log-page-button').count() === 0
    && /recent output 41999/.test(await page.innerText('#test-log-read-result')));
  await page.click('#test-logs-dialog .dialog-close');
  daemon.setScenario(SCENARIOS.logPageError);
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await revealTestRows(page);
  await revealTestRows(page); await page.locator('button[data-test-logs]').first().click();
  await page.waitForSelector('#test-log-read-result pre.log');
  await waitForRenderFrame(page);
  await page.locator('#test-log-read-result').evaluate((element) => {
    element.scrollTop = 0; element.dispatchEvent(new Event('scroll'));
  });
  await page.waitForSelector('#test-log-read-result .log-page-error');
  check('tests: an earlier-page failure preserves visible output and offers one specific retry',
    /Earlier output is temporarily unavailable/.test(await page.innerText('.log-page-error'))
    && /assertion failed/.test(await page.innerText('#test-log-read-result'))
    && await page.locator('.log-page-error button').count() === 1);
  const beforePageRetry = daemon.calls.length;
  await page.click('.log-page-error button');
  await waitForSettledCall(daemon, page, (call) => daemon.calls.indexOf(call) >= beforePageRetry
    && call.operation === 'test.log.tail' && call.params.cursor === 'older-tail');
  await page.waitForFunction(() => document.querySelector('#test-log-read-result')?.textContent.includes('earlier setup output 1'));
  check('interaction: retry resumes the same cursor-driven infinite-scroll page',
    daemon.calls.filter((call) => call.operation === 'test.log.tail' && call.params.cursor === 'older-tail').length === 2);
  await page.click('#test-logs-dialog .dialog-close');
  daemon.setScenario(SCENARIOS.logReadError);
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await revealTestRows(page);
  await revealTestRows(page); await page.locator('button[data-test-logs]').first().click();
  await page.waitForSelector('#test-log-retry');
  check('tests: an expired first read explains the failure and offers a real retry',
    /This retained log expired/.test(await page.innerText('#test-log-read-result')));
  await page.click('#test-log-retry');
  await page.waitForSelector('#test-log-read-result pre.log');
  check('interaction: retry recovers through the same bounded automatic read',
    daemon.calls.filter((call) => call.operation === 'test.log.tail').length === 2
    && /assertion failed/.test(await page.innerText('#test-log-read-result')));
  await page.click('#test-logs-dialog .dialog-close');
  daemon.setScenario(SCENARIOS.populated);
  await page.goto(`http://${HOST}:${port}/#/tests?repository=${REPO}`);
  await revealTestSettings(page); await page.waitForSelector('#test-log-retention-open');
  await revealTestSettings(page); await page.click('#test-log-retention-open');
  await page.waitForSelector('dialog#test-log-retention-dialog[open]');
  const narrowRetention = await page.locator('#test-log-retention-dialog').boundingBox();
  check('tests: Log retention remains visible and contained at the narrow viewport', narrowRetention
    && narrowRetention.x >= -1 && narrowRetention.y >= -1
    && narrowRetention.x + narrowRetention.width <= VIEWPORTS.narrow.width + 1
    && narrowRetention.y + narrowRetention.height <= VIEWPORTS.narrow.height + 1,
  JSON.stringify(narrowRetention));
  await page.click('[data-retention-cancel]');
  await page.setViewportSize(VIEWPORTS.wide);
  await revealTestSettings(page); await page.click('#test-capacity-open');
  await page.waitForSelector('dialog#test-capacity-dialog[open]');
  const capacityText = await page.innerText('#test-capacity-dialog');
  check('tests: capacity dialog shows measured scheduler state and the last adjustment',
    /Auto capacity\s+96/.test(capacityText)
    && /Effective\s+80/.test(capacityText)
    && /Maximum\s+80/.test(capacityText)
    && /Active\s+52/.test(capacityText)
    && /Waiting\s+11/.test(capacityText)
    && /Admission\s+open/.test(capacityText)
    && /Increased after a saturated, underused epoch/.test(capacityText), capacityText);
  daemon.calls.length = 0;
  await page.fill('#test-capacity-form [name=cap]', '72');
  await page.click('#test-capacity-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'test.capacity.set');
  check('interaction: saving the administrator maximum calls test.capacity.set directly',
    daemon.calls.some((call) => call.operation === 'test.capacity.set' && call.params.cap === 72));
  await revealTestSettings(page); await page.waitForSelector('#test-capacity-open');
  check('interaction: saving capacity returns focus to the Capacity action',
    await page.locator('#nav-toggle:focus').count() === 1);
  await revealTestSettings(page); await page.click('#test-capacity-open');
  await page.waitForSelector('dialog#test-capacity-dialog[open]');
  daemon.calls.length = 0;
  await page.click('#test-capacity-clear');
  await waitForSettledCall(daemon, page, 'test.capacity.set');
  check('interaction: clearing the administrator maximum sends an explicit null cap',
    daemon.calls.some((call) => call.operation === 'test.capacity.set' && call.params.cap === null));
  await page.waitForSelector('[data-test-start]', { state: 'attached' });
  check('interaction: clearing capacity returns focus to the Capacity action',
    await page.locator('#nav-toggle:focus').count() === 1);
  await revealTestRows(page);
  await chooseRepository(page, LONG);
  await page.click('#test-run-open');
  await page.waitForSelector('#test-run-form');
  const tierControl = page.locator('#test-run-form [name=tier]');
  check('tests: inline run form offers all three tiers with release as the default',
    JSON.stringify(await tierControl.locator('option').allTextContents()) === JSON.stringify(['Release', 'Pre-merge', 'Development'])
    && await tierControl.inputValue() === 'release');
  daemon.calls.length = 0;
  await tierControl.selectOption('pre-merge');
  await page.click('#test-run-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'test.start');
  check('interaction: starting a test sends the selected validation tier',
    daemon.calls.some((call) => call.operation === 'test.start' && call.params.tier === 'pre-merge'));

  // The visual evidence workspace uses a real capture as its fake-daemon image
  // so geometry, drawing, zoom, responsive layout, and image chunk assembly are
  // exercised against realistic pixels rather than a placeholder.
  await page.setViewportSize(EVIDENCE_WIDE);
  await page.evaluate(() => document.querySelector('#toasts')?.replaceChildren());
  const testedPage = await page.screenshot({ type: 'png' });
  daemon.setEvidenceImage(testedPage, EVIDENCE_WIDE.width, EVIDENCE_WIDE.height);
  daemon.calls.length = 0;
  await page.goto(`http://${HOST}:${port}/#/tests/${TEST_RUN}`);
  await page.waitForSelector('#evidence-image:not([hidden])');
  await page.waitForFunction(() => document.querySelector('#evidence-canvas')?.dataset.draftCount === '0');
  check('visual evidence: metadata loads before bounded image chunks',
    daemon.calls.findIndex((call) => call.operation === 'test.evidence.get') >= 0
    && daemon.calls.findIndex((call) => call.operation === 'test.evidence.image')
      > daemon.calls.findIndex((call) => call.operation === 'test.evidence.get'));
  check('visual evidence: the real screenshot is primary with journey and capture context',
    await page.locator('.evidence-step').count() === 2
    && await page.locator('#evidence-image').count() === 1
    && /Step 1 of 2/.test(await page.innerText('#evidence-current'))
    && /Capture details/.test(await page.innerText('#evidence-inspector')));
  await page.screenshot({ path: path.join(OUT, 'test-evidence-review-wide.png'), fullPage: true });

  await page.click('[data-evidence-step="step-2"]');
  await page.waitForFunction(() => /Step 2 of 2/.test(document.querySelector('#evidence-current')?.textContent || ''));
  check('interaction: selecting a journey step updates locally without another metadata read',
    daemon.calls.filter((call) => call.operation === 'test.evidence.get').length === 1
    && /Invalid password/.test(await page.innerText('#evidence-current')));
  await page.click('[data-evidence-prev]');
  await page.waitForFunction(() => /Step 1 of 2/.test(document.querySelector('#evidence-current')?.textContent || ''));
  await page.click('[data-evidence-next]');
  await page.waitForFunction(() => /Step 2 of 2/.test(document.querySelector('#evidence-current')?.textContent || ''));
  check('interaction: previous and next controls traverse the same ordered journey locally',
    daemon.calls.filter((call) => call.operation === 'test.evidence.get').length === 1);
  await page.click('[data-evidence-viewport="mobile"]');
  await page.waitForFunction(() => /390 × 844/.test(document.querySelector('#evidence-inspector')?.textContent || ''));
  check('interaction: viewport comparison keeps the same journey moment',
    /Invalid password/.test(await page.innerText('#evidence-current'))
    && await page.locator('.evidence-variant.active').count() === 1);
  await page.click('[data-evidence-viewport="desktop"]');
  await page.waitForSelector('#evidence-image:not([hidden])');
  await page.click('[data-evidence-kind="full_page"]');
  await page.waitForSelector('#evidence-image:not([hidden])');
  check('interaction: full-page evidence switches without re-reading the journey manifest',
    daemon.calls.filter((call) => call.operation === 'test.evidence.get').length === 1);
  await page.click('[data-evidence-kind="viewport"]');
  await page.waitForSelector('#evidence-image:not([hidden])');
  await page.click('[data-evidence-finding="insufficient-text-contrast"]');
  check('interaction: an automatic finding returns focus to its exact capture',
    await page.locator('[data-evidence-finding].active').count() === 1
    && await page.locator('#evidence-canvas:focus').count() === 1);

  const draw = async (tool, from, to = null) => {
    await page.click(`[data-evidence-tool="${tool}"]`);
    const box = await page.locator('#evidence-canvas').boundingBox();
    const start = { x: box.x + box.width * from[0], y: box.y + box.height * from[1] };
    if (!to) { await page.mouse.click(start.x, start.y); return; }
    const finish = { x: box.x + box.width * to[0], y: box.y + box.height * to[1] };
    await page.mouse.move(start.x, start.y); await page.mouse.down();
    await page.mouse.move(finish.x, finish.y, { steps: 6 }); await page.mouse.up();
  };
  await draw('pin', [.2, .2]);
  await draw('rectangle', [.32, .3], [.55, .44]);
  await draw('arrow', [.65, .2], [.55, .34]);
  await draw('freehand', [.18, .62], [.42, .67]);
  await draw('highlight', [.52, .62], [.78, .62]);
  await draw('text', [.35, .78]);
  await page.fill('.evidence-text-entry', 'Needs more space');
  await page.press('.evidence-text-entry', 'Enter');
  check('interaction: every drawing tool creates editable normalized markup',
    await page.getAttribute('#evidence-canvas', 'data-draft-count') === '6');
  await page.click('[data-evidence-tool="select"]');
  let canvasBox = await page.locator('#evidence-canvas').boundingBox();
  await page.mouse.click(canvasBox.x + canvasBox.width * .42, canvasBox.y + canvasBox.height * .36);
  check('interaction: Select targets a draft annotation',
    Boolean(await page.getAttribute('#evidence-canvas', 'data-selected-mark')));
  await page.keyboard.press('ArrowRight');
  await page.mouse.move(canvasBox.x + canvasBox.width * .42, canvasBox.y + canvasBox.height * .36);
  await page.mouse.down();
  await page.mouse.move(canvasBox.x + canvasBox.width * .47, canvasBox.y + canvasBox.height * .41, { steps: 5 });
  await page.mouse.up();
  await page.mouse.move(canvasBox.x + canvasBox.width * .60, canvasBox.y + canvasBox.height * .49);
  await page.mouse.down();
  await page.mouse.move(canvasBox.x + canvasBox.width * .70, canvasBox.y + canvasBox.height * .56, { steps: 5 });
  await page.mouse.up();
  check('interaction: selected marks support pointer move, resize, and keyboard nudging',
    await page.getAttribute('#evidence-canvas', 'data-draft-count') === '6');
  await page.mouse.click(canvasBox.x + canvasBox.width * .35, canvasBox.y + canvasBox.height * .78);
  await page.keyboard.press('Delete');
  check('interaction: Delete removes only the selected unsaved mark',
    await page.getAttribute('#evidence-canvas', 'data-draft-count') === '5');
  await page.click('[data-evidence-undo]');
  check('interaction: undo restores the deleted draft mark',
    await page.getAttribute('#evidence-canvas', 'data-draft-count') === '6');
  await page.click('[data-evidence-redo]');
  check('interaction: redo reapplies the draft deletion',
    await page.getAttribute('#evidence-canvas', 'data-draft-count') === '5');
  await page.click('[data-evidence-undo]');
  await page.click('[data-evidence-zoom-in]');
  const zoomedEvidenceValue = await page.textContent('#evidence-zoom-value');
  check('interaction: zoom changes the evidence canvas without changing the screenshot',
    zoomedEvidenceValue === '125%', zoomedEvidenceValue);
  canvasBox = await page.locator('#evidence-canvas').boundingBox();
  await page.locator('#evidence-canvas').focus();
  await page.keyboard.down('Space');
  await page.mouse.move(canvasBox.x + canvasBox.width * .75, canvasBox.y + canvasBox.height * .5);
  await page.mouse.down();
  await page.mouse.move(canvasBox.x + canvasBox.width * .45, canvasBox.y + canvasBox.height * .5, { steps: 6 });
  await page.mouse.up(); await page.keyboard.up('Space');
  check('interaction: Space-drag pans a zoomed screenshot',
    await page.evaluate(() => document.querySelector('#evidence-scroll').scrollLeft > 0));
  await page.click('[data-evidence-fit]');
  const fittedEvidenceValue = await page.textContent('#evidence-zoom-value');
  check('interaction: fit restores the full screenshot view',
    fittedEvidenceValue === '100%', fittedEvidenceValue);
  await page.click('[data-evidence-clear]');
  check('interaction: Clear removes only unsaved markup',
    await page.getAttribute('#evidence-canvas', 'data-draft-count') === '0');
  await page.selectOption('#evidence-color', '#ef4444');
  await draw('pin', [.55, .18]);
  await page.selectOption('#evidence-color', '#f59e0b');
  await draw('rectangle', [.30, .12], [.64, .20]);
  await page.selectOption('#evidence-color', '#4c8dff');
  await draw('arrow', [.76, .09], [.65, .16]);
  await page.fill('#evidence-feedback-create [name=body]', 'The primary action needs stronger contrast.');
  await page.click('#evidence-feedback-create button[type=submit]');
  await waitForSettledCall(daemon, page, 'test.evidence.feedback.create');
  const createdFeedback = daemon.calls.find((call) => call.operation === 'test.evidence.feedback.create');
  check('interaction: a marked suggestion creates screenshot feedback with normalized geometry',
    createdFeedback && createdFeedback.params.marks.length === 3
    && new Set(createdFeedback.params.marks.map((mark) => mark.type)).size === 3
    && createdFeedback.params.marks.some((mark) => mark.type === 'pin' && mark.color === '#ef4444')
    && createdFeedback.params.marks[0].x >= 0 && createdFeedback.params.marks[0].x <= 1);
  check('visual evidence: saved feedback exposes its real Plan continuation',
    await page.locator('[data-evidence-open-task]').count() === 1
    && /Discussion/.test(await page.innerText('#evidence-inspector')));
  await page.screenshot({ path: path.join(OUT, 'test-evidence-review-feedback-wide.png'), fullPage: true });
  await page.click('[data-evidence-open-task]');
  await page.waitForSelector('.plan-workspace');
  check('interaction: screenshot feedback opens its exact selected Plan task',
    /The primary action needs stronger contrast/.test(await page.innerText('.plan-selection')));
  await page.goto(`http://${HOST}:${port}/#/tests/${TEST_RUN}`);
  await page.waitForSelector('#evidence-image:not([hidden])');
  await page.click('[data-evidence-feedback]');
  await page.fill('#evidence-feedback-reply [name=body]', 'Please use the standard primary button treatment.');
  await page.click('#evidence-feedback-reply button[type=submit]');
  await waitForSettledCall(daemon, page, 'test.evidence.feedback.reply');
  await page.waitForFunction(() => document.querySelectorAll('.evidence-comment').length === 2);
  check('interaction: replies persist in the selected screenshot thread',
    await page.locator('.evidence-comment').count() === 2);
  await page.locator('[data-evidence-edit-comment]').first().click();
  await page.fill('.evidence-comment-edit [name=body]', 'The primary action needs the normal contrast.');
  await page.click('.evidence-comment-edit button[type=submit]');
  await waitForSettledCall(daemon, page, 'test.evidence.feedback.edit');
  check('interaction: the author can edit their saved wording',
    /normal contrast/.test(await page.innerText('.evidence-comment')));
  await page.click('[data-evidence-state="resolved"]');
  await waitForSettledCall(daemon, page, (call) => call.operation === 'test.evidence.feedback.state' && call.params.state === 'resolved');
  check('interaction: resolving feedback resolves its linked Plan work',
    /resolved/.test(await page.innerText('.evidence-thread-state')));
  await page.click('[data-evidence-state="open"]');
  await waitForSettledCall(daemon, page, (call) => call.operation === 'test.evidence.feedback.state' && call.params.state === 'open');
  check('interaction: resolved feedback can be reopened',
    /open/.test(await page.innerText('.evidence-thread-state')));
  await page.setViewportSize(VIEWPORTS.narrow);
  if (await page.locator('.evidence-layout-controls [data-evidence-panel=details]').getAttribute('aria-expanded') === 'true') await page.click('.evidence-layout-controls [data-evidence-panel=details]');
  await page.click('.evidence-layout-controls [data-evidence-panel=details]');
  const narrowEvidence = await page.evaluate(() => ({
    overflow: document.documentElement.scrollWidth - innerWidth,
    inspectorOpen: document.querySelector('.evidence-page')?.classList.contains('inspector-open'),
    canvasWidth: document.querySelector('#evidence-canvas')?.getBoundingClientRect().width,
  }));
  check('visual evidence: narrow layout keeps the canvas reachable beside dismissible feedback details',
    narrowEvidence.overflow <= 0 && narrowEvidence.inspectorOpen && narrowEvidence.canvasWidth > 250,
    JSON.stringify(narrowEvidence));
  await page.screenshot({ path: path.join(OUT, 'test-evidence-review-narrow.png'), fullPage: true });
  await page.setViewportSize(VIEWPORTS.wide);
  await page.click('[data-evidence-delete]');
  await waitForSettledCall(daemon, page, 'test.evidence.feedback.delete');
  check('interaction: the explicit delete action removes the annotation and drops its Plan task',
    await page.locator('.evidence-thread').count() === 0
    && daemon.calls.some((call) => call.operation === 'test.evidence.feedback.delete'));

  await page.goto(`http://${HOST}:${port}/#/bugs`);
  await page.waitForSelector('#bug-form');
  for (const [f, v] of [['component', 'api'], ['summary', 'verify'], ['expected', 'a'], ['actual', 'b'], ['steps', 'c']]) await page.fill(`#bug-form [name=${f}]`, v);
  await page.click('#bug-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'bug.report');
  check('interaction: bug report form calls bug.report', daemon.calls.some((c) => c.operation === 'bug.report' && c.params.summary === 'verify'));
  await page.goto(`http://${HOST}:${port}/#/admin`);
  await page.waitForSelector('#invite-form');
  await page.fill('#invite-form [name=email]', 'new2@example.test');
  await page.click('#invite-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'user.invite');
  check('interaction: invite form calls user.invite', daemon.calls.some((c) => c.operation === 'user.invite' && c.params.email === 'new2@example.test'));
  await page.waitForFunction(() => /daemon 0\.2\.0 · schema 16/.test(document.querySelector('#server')?.textContent || ''), null, { timeout: 10000 });
  check('admin: the Server line renders daemon version, schema, and route generation',
    /daemon 0\.2\.0 · schema 16 · route document generation 1/.test(await page.innerText('#server')),
    await page.innerText('#server'));
  await page.goto(`http://${HOST}:${port}/#/health/containers`);
  await page.waitForSelector('button[data-cmd="health.container_remove"]');
  const removable = await page.$$('button[data-cmd="health.container_remove"]');
  check('interaction: only orphaned/test containers offer removal', removable.length === 1);
  const observedContainer = page.locator('tr', { hasText: 'existing-compose-stack-app-1' });
  check('observed container is labelled and non-removable',
    /observed-current/.test(await observedContainer.innerText()) &&
    await observedContainer.locator('button[data-cmd="health.container_remove"]').count() === 0);
  await removable[0].click();
  await waitForSettledCall(daemon, page, 'health.container_remove');
  check('interaction: container removal calls health.container_remove with the exact id', daemon.calls.some((c) => c.operation === 'health.container_remove' && c.params.container_id === 'c'.repeat(64)));
  await page.goto(`http://${HOST}:${port}/#/health`);
  await page.waitForSelector('.card.bad-edge');
  const healthText = await page.innerText('body');
  check('health names the unhealthy component and its cause',
    /worker/.test(healthText) && /exited 1: boom/.test(healthText) && /healthcheck failing/.test(healthText));
  check('health offers actions on unhealthy deployments',
    await page.locator('.card.bad-edge [data-cmd="deployment.restart"]').count() >= 1);
  check('health keeps the container inventory and incident details directly reachable',
    await page.locator('.health-page-heading a[href="#/health/containers"]').count() === 1
    && await page.locator('.health-incident-card a[href^="#/deployments/"]').count() >= 2);
  await page.waitForSelector('.chartbox svg.chart');
  check('health charts render host history with a min–max band',
    await page.locator('.chartbox svg.chart .band').count() >= 3);
  daemon.calls.length = 0;
  await page.click('[data-health-range="7d"]');
  await page.waitForSelector('.chartbox svg.chart');
  await waitForSettledCall(daemon, page,
    (call) => call.operation === 'health.history' && call.params.minutes === 10080);
  check('interaction: the 7d range requests a downsampled week of host history',
    daemon.calls.some((c) => c.operation === 'health.history' && c.params.minutes === 10080 && c.params.points > 0));
  daemon.calls.length = 0;
  await page.click('[data-health-range="30d"]');
  await page.waitForSelector('.chartbox svg.chart');
  await waitForSettledCall(daemon, page,
    (call) => call.operation === 'health.history' && call.params.minutes === 43200);
  check('interaction: the 30d range requests a downsampled month of host history',
    daemon.calls.some((c) => c.operation === 'health.history' && c.params.minutes === 43200 && c.params.points > 0));
  for (const action of ['start', 'stop', 'restart']) {
    daemon.calls.length = 0;
    await page.click(`.health-incident-card [data-cmd="deployment.${action}"]`);
    await page.waitForSelector('.health-incident-card');
    await waitForSettledCall(daemon, page, `deployment.${action}`);
    check(`interaction: Health ${action} acts on the selected unhealthy deployment`,
      daemon.calls.some((call) => call.operation === `deployment.${action}` && call.params.deployment_id === 'd1111111111111111'));
  }
  await page.click('.health-page-heading a[href="#/health/containers"]');
  await page.waitForURL(/#\/health\/containers$/);
  await page.waitForFunction(() => document.querySelector('main h1 strong')?.textContent === 'Containers');
  await page.waitForSelector('table');
  check('interaction: View containers opens the real container inventory', /Containers/.test(await page.innerText('main h1')));
  await page.click('main a[href="#/health"]');
  await page.waitForURL(/#\/health$/);
  await page.waitForSelector('.health-incident-card');
  check('interaction: the container inventory returns to Health through its visible link', await page.locator('.health-summary').count() === 1);
  await page.click('.health-incident-card a.btn[href^="#/deployments/"]');
  await page.waitForURL(/#\/deployments\//);
  await page.waitForSelector('main h1 a[href="#/deployments"]');
  check('interaction: incident details opens the deployment detail route', await page.locator('main h1 a[href="#/deployments"]').count() === 1);
  daemon.setScenario(SCENARIOS.error);
  await page.goto(`http://${HOST}:${port}/#/health`);
  await page.waitForSelector('.notice .btn');
  daemon.setScenario(SCENARIOS.populated);
  await page.click('.notice .btn');
  await page.waitForSelector('.health-summary');
  check('interaction: Health recovers from a load failure through Retry', await page.locator('.health-summary').count() === 1);

  // Codex Usage: linked navigation, repository selection, truthful phase
  // chart, keyboard-preserving range changes, and accessible exact values.
  await page.goto(`http://${HOST}:${port}/#/usage/${REPO}`);
  await page.waitForSelector('.usage-phase-chart');
  check('usage: the shared workspace offers real same-aspect repository links',
    await page.locator('#repository-list a[href="#/usage/r2"]').count() === 1 && await page.locator('[data-project-picker]').count() === 0);
  await chooseRepository(page, LONG);
  await page.waitForURL(/#\/usage\/r2$/);
  check('usage: changing repositories preserves the Usage view', (await page.locator('#workspace-heading').innerText()) === LONG);
  await page.goto(`http://${HOST}:${port}/#/usage/${REPO}`);
  await page.waitForSelector('.usage-phase-chart');
  check('usage: provider total tokens are stacked by the six work phases',
    await page.locator('.usage-phase-chart rect').count() >= 6
    && /Planning/.test(await page.innerText('.usage-legend'))
    && /Unattributed/.test(await page.innerText('.usage-legend')));
  const axisTitleBox = await page.locator('.usage-axis-title').boundingBox();
  const topTickBox = await page.locator('.usage-y-label').last().boundingBox();
  check('usage: large scale labels cannot overlap the y-axis title',
    axisTitleBox && topTickBox && axisTitleBox.x + axisTitleBox.width + 4 <= topTickBox.x,
    JSON.stringify({ axisTitleBox, topTickBox }));
  check('usage: overlapping time measures remain separate rails',
    await page.locator('.usage-rail').count() === 3
    && /not added together/.test(await page.innerText('.usage-lower')));
  const usageContextText = await page.innerText('.usage-context');
  check('usage: missing-data status stays concise before the chart',
    /Some usage may be missing/.test(usageContextText)
    && /data from 3 of 4 configured Codex environments/.test(usageContextText)
    && !/separately configured local Codex setup with its own usage history/.test(usageContextText)
    && !/excluded, never counted as zero/.test(usageContextText)
    && await page.locator('.usage-coverage-popover[hidden]').count() === 1
    && await page.locator('.usage-coverage-note').count() === 0
    && !/\bcollectors?\b|Partial coverage|measured values only|configured histories/.test(usageContextText));
  const usageHintToggle = page.locator('[data-usage-coverage-hint-toggle]');
  check('usage: completeness hint has an accessible controlled dialog',
    await usageHintToggle.getAttribute('aria-controls') === 'usage-coverage-hint'
    && await usageHintToggle.getAttribute('aria-expanded') === 'false'
    && await page.locator('#usage-coverage-hint[role="dialog"][aria-labelledby="usage-coverage-hint-title"]').count() === 1);
  await usageHintToggle.focus();
  await page.keyboard.press('Enter');
  await page.waitForSelector('.usage-coverage-popover:not([hidden])');
  await page.waitForFunction(() => document.activeElement?.matches('.usage-coverage-popover'));
  const usageHintText = await page.innerText('.usage-coverage-popover');
  check('interaction: keyboard opens the full usage explanation only on request',
    await usageHintToggle.getAttribute('aria-expanded') === 'true'
    && await page.locator('.usage-coverage-popover:focus').count() === 1
    && /separately configured local Codex setup with its own usage history/.test(usageHintText)
    && /excluded, never counted as zero/.test(usageHintText));
  await page.keyboard.press('Escape');
  check('interaction: Escape closes the usage hint and returns focus',
    await page.locator('.usage-coverage-popover[hidden]').count() === 1
    && await usageHintToggle.getAttribute('aria-expanded') === 'false'
    && await page.locator('[data-usage-coverage-hint-toggle]:focus').count() === 1);
  await usageHintToggle.click();
  await page.waitForSelector('.usage-coverage-popover:not([hidden])');
  await page.click('.usage-repo-mark');
  check('interaction: clicking outside dismisses the usage hint',
    await page.locator('.usage-coverage-popover[hidden]').count() === 1
    && await usageHintToggle.getAttribute('aria-expanded') === 'false');
  daemon.calls.length = 0;
  await page.click('[data-codex-range="7d"]');
  await page.waitForSelector('.usage-phase-chart');
  await waitForSettledCall(daemon, page,
    (call) => call.operation === 'usage.repository' && call.params.range === '7d');
  check('interaction: the 7d range re-reads the selected repository and restores focus',
    daemon.calls.some((c) => c.operation === 'usage.repository' && c.params.repository_id === REPO && c.params.range === '7d')
    && await page.locator('[data-codex-range="7d"]:focus').count() === 1);
  await page.click('.usage-provenance summary');
  check('interaction: coverage and provenance expands in place',
    await page.locator('.usage-provenance[open]').count() === 1);
  check('interaction: exact bucket values are available without hover',
    await page.locator('.usage-provenance[open] .usage-exact tbody tr').count() > 0);
  const completenessText = await page.innerText('.usage-provenance');
  check('usage: expanded details explain completeness and bucket status without collector jargon',
    /Data completeness/.test(completenessText)
    && /Counting method/.test(completenessText)
    && /Data status/.test(completenessText)
    && /Measured with gaps/.test(completenessText)
    && !/\bcollectors?\b|Partial coverage/.test(completenessText));

  for (const [scenarioName, scenario, expected, explanation] of [
    ['complete', SCENARIOS.usageComplete, 'All 4 configured Codex environments included', 'Every configured environment supplied measurable data for this repository and period.'],
    ['no-measurement', SCENARIOS.empty, 'No usage measured in this period', 'The connected environments contained no measured usage for this repository and period.'],
    ['unavailable', SCENARIOS.usageUnavailable, 'Usage data unavailable', 'Configured environments could not supply usage data for this repository and period.'],
  ]) {
    daemon.setScenario(scenario);
    await page.reload();
    await page.waitForSelector('[data-usage-coverage-hint-toggle]');
    await page.waitForFunction((expectedText) =>
      (document.querySelector('.usage-context')?.innerText || '').includes(expectedText), expected);
    const contextText = await page.innerText('.usage-context');
    check(`usage: ${scenarioName} state keeps its explanation hidden by default`,
      contextText.includes(expected) && !contextText.includes(explanation)
      && await page.locator('.usage-coverage-popover[hidden]').count() === 1
      && !/\bcollectors?\b|Partial coverage|Complete coverage|measured values only|configured histories/.test(contextText),
      contextText.slice(0, 240));
    await page.click('[data-usage-coverage-hint-toggle]');
    await page.waitForSelector('.usage-coverage-popover:not([hidden])');
    const explanationText = await page.innerText('.usage-coverage-popover');
    check(`interaction: ${scenarioName} hint explains what data is included`,
      explanationText.includes(explanation)
      && /separately configured local Codex setup with its own usage history/.test(explanationText)
      && !/\bcollectors?\b|Partial coverage|Complete coverage|measured values only|configured histories/.test(explanationText),
      explanationText.slice(0, 240));
    await page.keyboard.press('Escape');
  }
  daemon.setScenario(SCENARIOS.populated);

  // Progress: factual release work, truthful bars and running totals, local
  // selection, period reads, missing evidence, and exact Plan continuation.
  await page.goto(`http://${HOST}:${port}/#/progress/${REPO}`);
  await page.waitForSelector('.progress-pulse-chart');
  await chooseRepository(page, LONG);
  await page.waitForURL(/#\/progress\/r2$/);
  check('progress: changing repositories preserves Progress', (await page.locator('#workspace-heading').innerText()) === LONG);
  await page.goto(`http://${HOST}:${port}/#/progress/${REPO}`);
  await page.waitForSelector('.progress-pulse-chart');
  await page.evaluate(() => { window.__progressWorkspace = document.querySelector('[data-ui-region="progress-primary"]'); });
  daemon.calls.length = 0;
  for (const taskId of [P_C2, P_UNSIZED, P_G1]) {
    await page.click(`[data-progress-task="${taskId}"]`);
    check(`interaction: release-work row ${taskId} is selectable`,
      await page.locator(`[data-progress-task="${taskId}"].selected`).count() === 1);
  }
  check('interaction: selecting release work is local and keeps the page stable',
    await page.locator(`[data-progress-task="${P_G1}"].selected`).count() === 1
    && await page.evaluate(() => document.querySelector('[data-ui-region="progress-primary"]') === window.__progressWorkspace)
    && !daemon.calls.some((call) => call.operation === 'progress.repository')
    && !daemon.calls.some((call) => call.operation === 'task.update')
    && !/Priority queue|Release impact|dependency/.test(await page.innerText('main')));
  daemon.calls.length = 0;
  await page.click('[data-progress-period="hour"]');
  await waitForSettledCall(daemon, page,
    (call) => call.operation === 'progress.repository' && call.params.period === 'hour');
  check('interaction: hourly progress reads hourly repository buckets',
    daemon.calls.some((call) => call.operation === 'progress.repository'
      && call.params.repository_id === REPO && call.params.period === 'hour')
    && await page.locator('[data-progress-period="hour"]:focus').count() === 1
    && await page.locator('.progress-completed-bar').count() > 0
    && await page.locator('.progress-incoming-bar').count() > 0);
  daemon.calls.length = 0;
  await page.click('[data-progress-period="day"]');
  await waitForSettledCall(daemon, page,
    (call) => call.operation === 'progress.repository' && call.params.period === 'day');
  check('interaction: daily progress reads daily repository buckets',
    daemon.calls.some((call) => call.operation === 'progress.repository'
      && call.params.repository_id === REPO && call.params.period === 'day')
    && await page.locator('[data-progress-period="day"]:focus').count() === 1
    && await page.locator('.progress-completed-bar').count() > 0
    && await page.locator('.progress-incoming-bar').count() > 0);
  daemon.calls.length = 0;
  await page.click('[data-progress-period="week"]');
  await page.waitForSelector('.progress-pulse-chart');
  await waitForSettledCall(daemon, page,
    (call) => call.operation === 'progress.repository' && call.params.period === 'week');
  check('interaction: weekly progress re-reads aligned repository buckets and restores focus',
    daemon.calls.some((call) => call.operation === 'progress.repository'
      && call.params.repository_id === REPO && call.params.period === 'week')
    && await page.locator('[data-progress-period="week"]:focus').count() === 1
    && await page.locator('.progress-completed-bar').count() > 0
    && await page.locator('.progress-incoming-bar').count() > 0);
  await page.click('.progress-exact summary');
  check('interaction: exact progress values and counting rules expand in place',
    await page.locator('.progress-exact[open] tbody tr').count() === 8
    && /Planned lines added/.test(await page.innerText('.progress-exact'))
    && /estimate reductions and dropped work are excluded/.test(await page.innerText('.progress-exact'))
    && /not measured Git changes/.test(await page.innerText('.progress-exact')));
  await page.click(`.progress-actions a[href="#/plan/${REPO}"]`);
  await page.waitForURL(new RegExp(`#\\/plan\\/${REPO}$`));
  await page.waitForSelector('.gantt');
  check('interaction: Open full plan navigates to the repository plan',
    await page.locator('.gantt').count() === 1);
  await page.goto(`http://${HOST}:${port}/#/progress/${REPO}`);
  await page.waitForSelector('.progress-pulse-chart');
  daemon.setScenario(SCENARIOS.progressPartial);
  await page.reload();
  await page.waitForSelector('.progress-missing-note');
  check('progress: partial evidence is visible and never described as zero',
    /gaps stay blank/i.test(await page.innerText('.progress-pulse'))
    && /never counted as zero/.test(await page.innerText('.progress-missing-note')));
  daemon.setScenario(SCENARIOS.progressTokenPartial);
  await page.reload();
  await page.waitForSelector('[data-progress-evidence="tokens"] .progress-evidence-chart');
  const tokenLane = page.locator('[data-progress-evidence="tokens"]');
  const tokenCells = await page.locator('.progress-exact tbody tr td:nth-child(8)').allTextContents();
  check('progress: Token use shows the measured period total rather than the final bucket',
    /500K/.test(await tokenLane.innerText())
    && /Measured total; missing buckets stay blank/.test(await tokenLane.innerText())
    && await tokenLane.locator(':scope > strong[aria-label="Token use measured in this period: 500K"]').count() === 1,
  await tokenLane.innerText());
  check('progress: token gaps stay blank while a real observed zero remains measurable',
    JSON.stringify(tokenCells.map((value) => value.trim())) === JSON.stringify([
      '—', '120,000', '—', '0', '210,000', '170,000', '0'])
    && await tokenLane.locator('.progress-evidence-dot').count() === 5
    && await tokenLane.locator('.progress-evidence-line').count() === 2,
  JSON.stringify({ tokenCells, dots: await tokenLane.locator('.progress-evidence-dot').count(),
    segments: await tokenLane.locator('.progress-evidence-line').count() }));
  daemon.setScenario(SCENARIOS.progressReference);
  await page.reload();
  await page.waitForSelector('.progress-pulse-chart');
  const equalValueScale = await page.locator('.progress-bar-line-chart').evaluateAll((charts) => charts.every((chart) => {
    const completed = [...chart.querySelectorAll('.progress-completed-bar')];
    const incoming = [...chart.querySelectorAll('.progress-incoming-bar')];
    return completed.some((finished) => incoming.some((added) => (
      finished.getAttribute('x') === added.getAttribute('x')
      && Math.abs(Number(finished.getAttribute('height')) - Number(added.getAttribute('height'))) < .1
    )));
  }));
  check('progress: equal completed and incoming values have equal bar lengths', equalValueScale);
  await verifyProgressCharts({ page, daemon, check, scenario: SCENARIOS.progressReference,
    baseUrl: `http://${HOST}:${port}/`, output: OUT,
    settle: (period) => waitForSettledCall(daemon, page,
      (call) => call.operation === 'progress.repository' && call.params.period === period) });
  check('progress: entirely missing test and token evidence has no zero-valued chart',
    await page.locator('.progress-evidence-chart').count() === 0
    && await page.locator('.progress-evidence-lane > strong').allTextContents().then((values) => values.every((value) => value.trim() === '—'))
    && /No test runs recorded for this period/.test(await page.innerText('.progress-pulse'))
    && /Some token data is missing/.test(await page.innerText('.progress-pulse'))
    && /71 tasks have no estimate/.test(await page.innerText('.progress-forecast'))
    && /reopened/.test(await page.innerText('.progress-release-work'))
    && !/TECHNICAL-(?:UNBLOCK|REOPEN)-MARKER/.test(await page.innerText('.progress-release-work')));
  daemon.setScenario(SCENARIOS.populated);
  await page.reload();
  await page.waitForSelector('.progress-pulse-chart');
  await page.click(`[data-progress-task="${P_C2}"]`);
  await page.click('[data-progress-open-task]');
  await page.waitForURL(new RegExp(`#\\/plan\\/${REPO}$`));
  await page.waitForSelector(`[data-task-row="${P_C2}"].selected`);
  check('interaction: Open selected in plan continues to the exact real task',
    await page.locator(`[data-task-row="${P_C2}"].selected`).count() === 1);

  // Plan: immersive Gantt navigation, direct manipulation, accessible
  // alternatives, persistence, recovery, preview request, feedback, and drop.
  await page.goto(`http://${HOST}:${port}/#/plan/${REPO}`);
  await page.waitForSelector('.gantt');
  await chooseRepository(page, LONG);
  await page.waitForURL(/#\/plan\/r2$/);
  check('plan: shared repository selection keeps the current aspect', (await page.locator('#workspace-heading').innerText()) === LONG);
  await page.goto(`http://${HOST}:${port}/#/plan/${REPO}`);
  await page.waitForSelector('.gantt');
  check('plan: delivered preview links to the running app',
    await page.locator(`a[href="https://app-dev.${BASE}"]`).count() === 1);
  check('plan: a delivered preview without a domain names its server port',
    /runs on server port 20005/.test(await page.innerText('body')));
  check('plan: the selected task tray is visible without displacing the chart',
    await page.locator('[data-ui-region="selected-task"]').count() === 1
    && await page.locator('[data-ui-region="plan-primary"]').count() === 1);
  check('plan: the delivered task exposes no move, resize, or drag control',
    await page.locator(`[data-task-row="${P_D1}"] [data-drag-task], [data-resize-handle="${P_D1}"], [data-move-task="${P_D1}"]`).count() === 0);
  check('plan: work without an estimate still has a selectable chart mark',
    await page.locator(`.gbar.unsized[data-select-task="${P_UNSIZED}"]`).count() === 1
    && /Not estimated/.test(await page.innerText('.gaxis')));
  check('plan: every task row has an elaboration action, including completed work',
    await page.locator('.gtask [data-elaborate-task]').count() === await page.locator('.gtask').count());
  check('plan: an outstanding request is visible and cannot be submitted twice',
    await page.locator(`[data-task-row="${P_D1}"] [data-elaborate-task][aria-disabled="true"]`).count() === 1
    && /Requested/.test(await page.innerText(`[data-task-row="${P_D1}"]`)));

  daemon.calls.length = 0;
  await page.locator('.plan-workspace').evaluate((element) => { element.dataset.identityProof = 'same-workspace'; });
  await page.click(`[data-task-row="${P_C2}"] [data-elaborate-task]`);
  await page.waitForFunction((taskId) => document.querySelector(
    `[data-task-row="${taskId}"] [data-elaborate-task]`)?.getAttribute(
    'aria-disabled') === 'true', P_C2);
  check('interaction: requesting elaboration persists the flag and updates the existing Plan in place',
    daemon.calls.some((call) => call.operation === 'task.update' && call.params.task_id === P_C2 && call.params.elaboration_needed === true)
    && !daemon.calls.some((call) => call.operation === 'plan.overview')
    && await page.locator('.plan-workspace[data-identity-proof="same-workspace"]').count() === 1
    && /Requested/.test(await page.innerText(`[data-task-row="${P_C2}"]`))
    && /2 tasks need a clearer explanation/.test(await page.innerText('[data-plan-elaboration-notice]')),
    JSON.stringify(daemon.calls));
  daemon.calls.length = 0; daemon.failNextTaskUpdate();
  await page.click(`[data-task-row="${P_G1}"] [data-elaborate-task]`);
  await waitForSettledCall(daemon, page, 'task.update');
  check('interaction: a failed elaboration request reports the failure and stays retryable',
    await page.locator(`[data-task-row="${P_G1}"] [data-elaborate-task]:not(:disabled)`).count() === 1
    && !/elaboration needed/.test(await page.innerText(`[data-task-row="${P_G1}"]`))
    && (await page.locator('.toast.bad').allInnerTexts()).some((text) => /Could not request elaboration/.test(text)));
  daemon.calls.length = 0;
  await page.locator(`[data-task-row="${P_UNSIZED}"] .plan-task-select`).scrollIntoViewIfNeeded();
  const selectionScrollBefore = await page.locator('[data-plan-viewport]').evaluate((element) => ({ left: element.scrollLeft, top: element.scrollTop }));
  await page.click(`[data-task-row="${P_UNSIZED}"] .plan-task-select`);
  await page.waitForSelector(`[data-task-row="${P_UNSIZED}"].selected`);
  const selectionScrollAfter = await page.locator('[data-plan-viewport]').evaluate((element) => ({ left: element.scrollLeft, top: element.scrollTop }));
  check('interaction: selecting a task updates in place without re-reading or replacing the Plan',
    !daemon.calls.some((call) => call.operation === 'plan.overview')
    && await page.locator('.plan-workspace[data-identity-proof="same-workspace"]').count() === 1
    && selectionScrollAfter.left === selectionScrollBefore.left
    && await page.locator('.plan-loading,.skeleton').count() === 0,
    JSON.stringify({ calls: daemon.calls, selectionScrollBefore, selectionScrollAfter }));
  check('interaction: an unsized task offers an honest estimate action',
    /Not estimated yet/.test(await page.innerText('[data-ui-region="selected-task"]'))
    && await page.locator(`[data-resize-task="${P_UNSIZED}"]`, { hasText: 'Add estimate' }).count() === 1);

  await page.click('[data-plan-selection-toggle]');
  await page.waitForSelector('.plan-selection.collapsed');
  await page.click('[data-plan-selection-toggle]');
  await page.waitForSelector('.plan-selection:not(.collapsed)');
  check('interaction: collapsing selected details is local and keeps the same workspace',
    !daemon.calls.some((call) => call.operation === 'plan.overview')
    && await page.locator('.plan-workspace[data-identity-proof="same-workspace"]').count() === 1);

  await page.hover(`[data-hover-task="${P_G1}"]`);
  await page.waitForSelector('#plan-tooltip:not([hidden])');
  check('interaction: hovering a task bar reveals its anchored detail badge',
    /E-mail field checks its spelling/.test(await page.innerText('#plan-tooltip'))
    && /100/.test(await page.innerText('#plan-tooltip')));

  await page.click(`[data-task-row="${P_C2}"] .plan-task-select`);
  await page.waitForSelector(`[data-task-row="${P_C2}"].selected`);
  check('interaction: selecting a task synchronizes the row, bar, and detail tray',
    await page.locator(`[data-hover-task="${P_C2}"].selected`).count() === 1
    && /Wrong password message/.test(await page.innerText('[data-ui-region="selected-task"]')));
  await page.click('[data-plan-selection-toggle]');
  await page.waitForSelector('.plan-selection.collapsed');
  check('interaction: the selected-task tray collapses in place', await page.locator('.plan-selection.collapsed').count() === 1);
  await page.click('[data-plan-selection-toggle]');
  await page.waitForSelector('.plan-selection:not(.collapsed)');

  const chartWidthBefore = await page.locator('.plan-workspace').evaluate((el) => Number.parseFloat(getComputedStyle(el).getPropertyValue('--chart-width')));
  await page.click('[data-plan-zoom="in"]');
  const chartWidthAfter = await page.locator('.plan-workspace').evaluate((el) => Number.parseFloat(getComputedStyle(el).getPropertyValue('--chart-width')));
  check('interaction: zoom in expands the lines-of-code canvas', chartWidthAfter > chartWidthBefore, `${chartWidthBefore} -> ${chartWidthAfter}`);
  await page.click('[data-plan-zoom="fit"]');
  check('interaction: fit makes the complete chart match the visible canvas', (await page.innerText('#plan-zoom-value')) === 'Fit');
  await page.click('[data-plan-zoom="in"]');
  const planViewport = page.locator('[data-plan-viewport]');
  const viewportBox = await planViewport.boundingBox();
  await page.click('[data-plan-mode="pan"]');
  const scrollBeforePan = await planViewport.evaluate((el) => el.scrollLeft);
  await page.mouse.move(viewportBox.x + viewportBox.width - 120, viewportBox.y + 150);
  await page.mouse.down();
  await page.mouse.move(viewportBox.x + viewportBox.width - 360, viewportBox.y + 150, { steps: 5 });
  await page.mouse.up();
  const scrollAfterPan = await planViewport.evaluate((el) => el.scrollLeft);
  check('interaction: hand mode pans the timeline without moving the document', scrollAfterPan > scrollBeforePan, `${scrollBeforePan} -> ${scrollAfterPan}`);
  await page.click('[data-plan-mode="select"]');
  await planViewport.evaluate((el) => { el.scrollLeft = 0; });
  await page.waitForFunction(() => Number.parseFloat(document.querySelector('[data-plan-minimap-thumb]')?.style.left || '0') === 0);
  const scrollBeforeMinimap = await planViewport.evaluate((el) => el.scrollLeft);
  const minimapControl = page.locator('[data-plan-minimap]');
  const minimapBox = await minimapControl.boundingBox();
  await minimapControl.click({ position: { x: minimapBox.width * 0.85, y: minimapBox.height / 2 } });
  const scrollAfterMinimap = await planViewport.evaluate((el) => el.scrollLeft);
  check('interaction: the minimap changes horizontal position', scrollAfterMinimap !== scrollBeforeMinimap, `${scrollBeforeMinimap} -> ${scrollAfterMinimap}`);

  await page.click('[data-plan-nav-toggle]');
  await page.waitForFunction(() => { const el = document.querySelector('.plan-workspace'); return !!el && Number.parseFloat(getComputedStyle(el).getPropertyValue('--glabel')) === 0; });
  check('interaction: the task navigator collapses to give the chart the full width',
    await page.locator('.plan-workspace').evaluate((el) => Number.parseFloat(getComputedStyle(el).getPropertyValue('--glabel'))) === 0);
  await page.click('[data-plan-nav-toggle]');
  await page.waitForFunction(() => { const el = document.querySelector('.plan-workspace'); return !!el && Number.parseFloat(getComputedStyle(el).getPropertyValue('--glabel')) > 0; });
  const navigatorBefore = await page.locator('.plan-workspace').evaluate((el) => Number.parseFloat(getComputedStyle(el).getPropertyValue('--glabel')));
  await page.focus('[data-plan-nav-resizer]');
  await page.keyboard.press('ArrowRight');
  const navigatorAfter = await page.locator('.plan-workspace').evaluate((el) => Number.parseFloat(getComputedStyle(el).getPropertyValue('--glabel')));
  check('interaction: the navigator divider resizes with the keyboard', navigatorAfter > navigatorBefore, `${navigatorBefore} -> ${navigatorAfter}`);

  const rowsBefore = await page.locator('.gtask:not(.ghidden)').count();
  daemon.calls.length = 0;
  await page.click(`[data-collapse="${P_C1}"]`);
  await page.waitForSelector('.gtask.ghidden', { state: 'attached' });
  check('plan: collapsing a parent hides its subtree rows',
    (await page.locator('.gtask:not(.ghidden)').count()) === rowsBefore - 1);
  await page.click(`[data-collapse="${P_C1}"]`);
  check('interaction: expanding and collapsing a task is local and keeps the same workspace',
    !daemon.calls.some((call) => call.operation === 'plan.overview')
    && await page.locator('.plan-workspace[data-identity-proof="same-workspace"]').count() === 1);

  daemon.calls.length = 0;
  await page.click(`[data-task-row="${P_UNSIZED}"] .plan-task-select`);
  await page.click(`[data-resize-task="${P_UNSIZED}"]`);
  await page.waitForSelector('dialog#estimate-dialog[open]');
  await page.fill('#estimate-form [name=estimated_loc]', '240');
  await page.click('#estimate-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'task.update');
  check('interaction: adding an estimate persists it and moves the task onto the measured scale',
    daemon.calls.some((call) => call.operation === 'task.update' && call.params.task_id === P_UNSIZED && call.params.estimated_loc === 240)
    && /~240 lines/.test(await page.innerText(`[data-task-row="${P_UNSIZED}"]`))
    && await page.locator(`.gbar.unsized[data-select-task="${P_UNSIZED}"]`).count() === 0);

  // Pointer resize persists through a full reload.
  await page.click(`[data-task-row="${P_C2}"] .plan-task-select`);
  await page.waitForSelector(`[data-resize-handle="${P_C2}"]`);
  daemon.calls.length = 0;
  await page.locator(`[data-resize-handle="${P_C2}"]`).scrollIntoViewIfNeeded();
  let resizeBox = await page.locator(`[data-resize-handle="${P_C2}"]`).boundingBox();
  await page.mouse.move(resizeBox.x + resizeBox.width / 2, resizeBox.y + resizeBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(resizeBox.x + resizeBox.width / 2 + 70, resizeBox.y + resizeBox.height / 2, { steps: 5 });
  await page.mouse.up();
  await waitForSettledCall(daemon, page, 'task.update');
  const pointerResizeCall = daemon.calls.find((c) => c.operation === 'task.update' && c.params.task_id === P_C2 && c.params.estimated_loc > 200);
  check('interaction: dragging the selected bar handle updates its real estimate', !!pointerResizeCall, JSON.stringify(pointerResizeCall?.params));
  const persistedEstimate = pointerResizeCall?.params.estimated_loc;
  await page.reload();
  await page.waitForSelector(`[data-task-row="${P_C2}"]`);
  check('interaction: resized task estimate survives reload', persistedEstimate && (await page.innerText(`[data-task-row="${P_C2}"]`)).includes(persistedEstimate.toLocaleString('en-US')));

  // Escape cancels pointer resize without an API write.
  await page.click(`[data-task-row="${P_C2}"] .plan-task-select`);
  await page.waitForSelector(`[data-resize-handle="${P_C2}"]`);
  daemon.calls.length = 0;
  await page.locator(`[data-resize-handle="${P_C2}"]`).scrollIntoViewIfNeeded();
  resizeBox = await page.locator(`[data-resize-handle="${P_C2}"]`).boundingBox();
  await page.mouse.move(resizeBox.x + resizeBox.width / 2, resizeBox.y + resizeBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(resizeBox.x + resizeBox.width / 2 + 40, resizeBox.y + resizeBox.height / 2, { steps: 3 });
  await page.keyboard.press('Escape');
  await page.mouse.up();
  await waitForRenderFrame(page);
  check('interaction: Escape cancels a pointer resize without changing the task', !daemon.calls.some((c) => c.operation === 'task.update'));

  // Failed resize keeps the persisted value and reports the error.
  await page.click(`[data-task-row="${P_C2}"] .plan-task-select`);
  await page.waitForSelector(`[data-resize-handle="${P_C2}"]`);
  daemon.calls.length = 0; daemon.failNextTaskUpdate();
  await page.locator(`[data-resize-handle="${P_C2}"]`).scrollIntoViewIfNeeded();
  resizeBox = await page.locator(`[data-resize-handle="${P_C2}"]`).boundingBox();
  await page.mouse.move(resizeBox.x + resizeBox.width / 2, resizeBox.y + resizeBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(resizeBox.x + resizeBox.width / 2 + 30, resizeBox.y + resizeBox.height / 2, { steps: 3 });
  await page.mouse.up();
  await waitForSettledCall(daemon, page, 'task.update');
  await page.waitForFunction((taskId) => (
    [...document.querySelectorAll('.toast.bad')].some((toast) => /resize failed/.test(toast.textContent || ''))
    && [...document.querySelectorAll('[data-task-row]')].some((row) => row.dataset.taskRow === taskId)
  ), P_C2);
  const resizeFailureText = await page.locator('.toast.bad').allInnerTexts();
  check('interaction: a failed resize reports failure and keeps the task available', resizeFailureText.some((text) => /resize failed/.test(text)) && await page.locator(`[data-task-row="${P_C2}"]`).count() === 1, resizeFailureText.join(' | '));

  // The numeric dialog is the keyboard/touch resize path.
  await page.click(`[data-task-row="${P_C2}"] .plan-task-select`);
  await page.waitForSelector(`[data-resize-task="${P_C2}"]`);
  daemon.calls.length = 0;
  await page.click(`[data-resize-task="${P_C2}"]`);
  await page.waitForSelector('dialog#estimate-dialog[open]');
  await page.fill('#estimate-form [name=estimated_loc]', '275');
  await page.click('#estimate-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'task.update');
  check('interaction: the resize dialog saves an exact estimate', daemon.calls.some((c) => c.operation === 'task.update' && c.params.task_id === P_C2 && c.params.estimated_loc === 275));

  if (await page.locator('.plan-selection:not(.collapsed) [data-plan-selection-toggle]').count()) {
    await page.click('[data-plan-selection-toggle]');
    await page.waitForSelector('.plan-selection.collapsed');
  }
  daemon.calls.length = 0;
  await page.evaluate(() => {
    window.__planDragEvents = [];
    for (const type of ['dragstart', 'dragenter', 'dragover', 'drop', 'dragend']) {
      document.addEventListener(type, (event) => window.__planDragEvents.push({
        type, target: event.target?.className || event.target?.tagName || '',
        row: event.target?.closest?.('[data-task-row]')?.dataset.taskRow || null,
      }), true);
    }
  });
  const dragSourceHit = await page.locator(`[data-drag-task="${P_C2}"]`).evaluate((element) => {
    const box = element.getBoundingClientRect();
    const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    return { box: { x: box.x, y: box.y, width: box.width, height: box.height },
      hit: hit?.className || hit?.tagName || '' };
  });
  await pointerDrag(`[data-drag-task="${P_C2}"]`, `[data-task-row="${P_C1}"]`, { x: 100, y: 28 });
  await waitForSettledCall(daemon, page, 'task.update');
  const reorderCall = daemon.calls.find((c) => c.operation === 'task.update');
  const dragEvents = await page.evaluate(() => window.__planDragEvents);
  const reorderedInView = reorderCall ? await page.waitForFunction(({ movedId, targetId }) => {
    const rows = [...document.querySelectorAll('[data-task-row]')].map((row) => row.dataset.taskRow);
    return rows.indexOf(movedId) >= 0 && rows.indexOf(movedId) < rows.indexOf(targetId);
  }, { movedId: P_C2, targetId: P_C1 }, { timeout: 2500 }).then(() => true).catch(() => false) : false;
  check('interaction: dragging a task above a sibling reorders it in place',
    reorderCall && reorderCall.params.task_id === P_C2 && reorderCall.params.position === 0
    && !('release_id' in reorderCall.params) && reorderedInView,
    JSON.stringify({ args: reorderCall?.params, reorderedInView, dragSourceHit, dragEvents }));
  daemon.calls.length = 0;
  await page.evaluate(({ sourceId, releaseId }) => {
    const viewport = document.querySelector('[data-plan-viewport]');
    const source = document.querySelector(`[data-drag-task="${sourceId}"]`);
    const sourceRow = source?.closest('[data-task-row]');
    const target = document.querySelector(`[data-drop-release="${releaseId}"]`);
    if (viewport && sourceRow && target) viewport.scrollTop = Math.max(0, ((sourceRow.offsetTop + target.offsetTop) / 2) - (viewport.clientHeight / 2));
  }, { sourceId: P_C2, releaseId: V_R2 });
  await pointerDrag(`[data-drag-task="${P_C2}"]`, `[data-drop-release="${V_R2}"]`, { x: 45 });
  await waitForSettledCall(daemon, page, 'task.update');
  const dragMoveCall = daemon.calls.find((c) => c.operation === 'task.update' && c.params.task_id === P_C2);
  check('interaction: dropping a task on a release header moves it into that release',
    dragMoveCall && dragMoveCall.params.task_id === P_C2 && dragMoveCall.params.release_id === V_R2,
    JSON.stringify(daemon.calls.filter((call) => call.operation === 'task.update').map((call) => call.params)));

  // Cancel is truthful before exercising the successful move path.
  daemon.calls.length = 0;
  await page.click(`[data-task-row="${P_G1}"] .plan-task-select`);
  await page.click(`[data-move-task="${P_G1}"]`);
  await page.waitForSelector('dialog#move-dialog[open]');
  await page.click('#move-cancel');
  check('interaction: cancelling the move dialog makes no API call', !daemon.calls.some((c) => c.operation === 'task.update'));
  daemon.calls.length = 0;
  await page.click(`[data-move-task="${P_G1}"]`);
  await page.waitForSelector('dialog#move-dialog[open]');
  await page.selectOption('#move-form [name=release_id]', V_R2);
  await page.click('#move-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'task.update');
  const dialogMoveCall = daemon.calls.find((c) => c.operation === 'task.update');
  check('interaction: the move pop-up posts the chosen release',
    dialogMoveCall && dialogMoveCall.params.task_id === P_G1 && dialogMoveCall.params.release_id === V_R2,
    JSON.stringify(dialogMoveCall?.params));
  daemon.calls.length = 0;
  await page.click('button[data-cmd="release.request"]');
  await waitForSettledCall(daemon, page, 'release.request');
  await page.waitForFunction(() => /Preview requested/.test(
    document.querySelector('main')?.textContent || ''));
  check('interaction: Request preview now calls release.request for the repository',
    daemon.calls.some((c) => c.operation === 'release.request' && c.params.repository_id === REPO));
  check('interaction: preview request re-renders as a pending notice', /Preview requested/.test(await page.innerText('main')));
  daemon.calls.length = 0;
  await page.click('[data-plan-feedback]');
  await page.waitForSelector('dialog#feedback-dialog[open]');
  await page.fill('#comment-form [name=title]', 'The export button fails for me');
  await page.click('#comment-form button[type=submit]');
  await waitForSettledCall(daemon, page, 'task.create');
  const feedbackCall = daemon.calls.find((c) => c.operation === 'task.create');
  check('interaction: the ask-for-a-change form creates a user_feedback task',
    feedbackCall && feedbackCall.params.title === 'The export button fails for me'
    && feedbackCall.params.kind === 'user_feedback' && feedbackCall.params.repository_id === REPO,
    JSON.stringify(feedbackCall?.params));
  check('interaction: submitted owner feedback appears in the plan', /The export button fails for me/.test(await page.innerText('main')));
  daemon.calls.length = 0;
  await page.click(`[data-task-row="${P_G1}"] .plan-task-select`);
  await page.click(`.plan-selection [data-cmd="task.update"]`);
  await waitForSettledCall(daemon, page, 'task.update');
  const dropCall = daemon.calls.find((c) => c.operation === 'task.update');
  check('interaction: the explicitly labelled drop action marks the task dropped immediately',
    dropCall && dropCall.params.task_id === P_G1 && dropCall.params.status === 'dropped',
    JSON.stringify(dropCall?.params));
  check('interaction: dropped task leaves the current plan after the state re-read', await page.locator(`[data-task-row="${P_G1}"]`).count() === 0);

  daemon.setScenario({ ...SCENARIOS.populated, densePlan: true });
  await page.reload();
  await page.waitForFunction(() => document.querySelectorAll('.gtask').length >= 120, null, { timeout: 15000 });
  check('plan: a verification-heavy plan renders every unknown-size job in the chart',
    await page.locator('.gtask .gbar.unsized:not(.parent)').count() === 111
    && await page.locator('.gtask').count() >= 120,
    `${await page.locator('.gtask .gbar.unsized:not(.parent)').count()} unknown marks for ${await page.locator('.gtask').count()} rows`);
  check('plan: the dense unknown-size plan keeps one honest non-proportional band',
    /Not estimated/.test(await page.innerText('.gaxis'))
    && /111 jobs not estimated/.test(await page.innerText('.plan-total-progress')));

  // Decisions: story, aspect filter, search, paging, superseded handling.
  daemon.setScenario(SCENARIOS.populated);
  await page.goto(`http://${HOST}:${port}/#/decisions/${REPO}`);
  await page.waitForSelector('.decision');
  await chooseRepository(page, LONG);
  await page.waitForURL(/#\/decisions\/r2$/);
  check('decisions: shared repository selection keeps the current aspect', (await page.locator('#workspace-heading').innerText()) === LONG);
  await page.goto(`http://${HOST}:${port}/#/decisions/${REPO}`);
  await page.waitForSelector('.decision');
  check('decisions: the story so far and the plain entries render',
    /The story so far/.test(await page.innerText('body')) && /Buttons are green now/.test(await page.innerText('body')));
  check('decisions: superseded entries are collapsed', await page.locator('details.decision.superseded:not([open])').count() === 1);
  daemon.calls.length = 0;
  await page.click('#decisions-older');
  await waitForSettledCall(daemon, page, 'decision.tail');
  check('interaction: Show older pages the tail with before_seq of the oldest shown decision',
    daemon.calls.some((c) => c.operation === 'decision.tail' && c.params.before_seq === 41));
  daemon.calls.length = 0;
  await page.click('[data-decision-aspect="ui"]');
  await waitForSettledCall(daemon, page, 'decision.tail');
  check('interaction: the aspect filter is applied server-side',
    daemon.calls.some((c) => c.operation === 'decision.tail' && c.params.aspect === 'ui' && !('before_seq' in c.params)));
  daemon.calls.length = 0;
  await page.fill('#decision-search [name=q]', 'export');
  await page.click('#decision-search button[type=submit]');
  await page.waitForSelector('text=REPO-EXPORT-FILES');
  check('interaction: search calls decision.search with the typed query',
    daemon.calls.some((c) => c.operation === 'decision.search' && c.params.query === 'export' && c.params.aspect === 'ui'));
  check('administrator actions never open a native confirmation dialog', nativeDialogCount === 0, `${nativeDialogCount} native dialogs`);
  await context.close();

  // Deployments dashboard: repository attribution, every responsive grid
  // transition, and multiple small mobile widths. No summary value or action
  // may cross into another repository or require horizontal page movement.
  daemon.setScenario(SCENARIOS.populated);
  const deploymentContext = await browser.newContext({ viewport: { width: 1440, height: 1024 } });
  const { cookie: deploymentCookie } = sessions.issue({ sub: 'sub', email: 'owner@example.test' });
  await deploymentContext.addCookies([{ name: 'dc2_session', value: deploymentCookie.split(';')[0].split('=')[1], domain: `.${BASE}`, path: '/' }]);
  const deploymentPage = await deploymentContext.newPage();
  const deploymentSamples = [
    { width: 320, height: 780 },
    { width: 390, height: 844 },
    { width: 430, height: 900 },
    { width: 619, height: 900 },
    { width: 620, height: 900 },
    { width: 621, height: 900 },
    { width: 959, height: 960 },
    { width: 960, height: 960 },
    { width: 961, height: 960 },
    { width: 1095, height: 876 },
    { width: 834, height: 1194 },
    { width: 1179, height: 900 },
    { width: 1180, height: 900 },
    { width: 1181, height: 900 },
    { width: 1239, height: 900 },
    { width: 1240, height: 900 },
    { width: 1241, height: 900 },
    { width: 1440, height: 1024 },
  ];
  for (const viewport of deploymentSamples) {
    await deploymentPage.setViewportSize(viewport);
    await deploymentPage.goto(`http://${HOST}:${port}/#/deployments?repository=${REPO}`);
    await deploymentPage.waitForSelector('.deployment-record');
    const layout = await deploymentPage.evaluate(() => ({
      overflow: document.documentElement.scrollWidth - innerWidth,
      columns: getComputedStyle(document.querySelector('.deployment-record')).gridTemplateColumns.split(' ').length,
      clipped: [...document.querySelectorAll('.deployment-record-identity, .deployment-record-facts dd')].filter(element => element.scrollWidth > element.clientWidth + 1).map(element => element.className),
      controls: [...document.querySelectorAll('.deployment-record a, .deployment-record button')].filter(element => element.getClientRects().length).every(element => element.getBoundingClientRect().right <= innerWidth + 1),
    }));
    const expectedColumns = viewport.width <= 540 ? 1 : viewport.width <= 1100 ? 2 : 4;
    check(`deployments ${viewport.width}px: selected repository fits the pane with every action reachable`, layout.overflow <= 0 && layout.columns === expectedColumns && layout.clipped.length === 0 && layout.controls, JSON.stringify(layout));
    await deploymentPage.screenshot({ path: path.join(OUT, `deployments-${viewport.width}.png`), fullPage: true });
  }
  await deploymentContext.close();

  // Health layout: preserve the user-marked 856 px surface, mobile, desktop,
  // and the exact responsive transition where the repository table becomes
  // labelled cards. Every storage category must remain visible without a
  // horizontal discovery path.
  daemon.setScenario(SCENARIOS.populated);
  const healthContext = await browser.newContext({ viewport: { width: 1440, height: 1024 } });
  const { cookie: healthCookie } = sessions.issue({ sub: 'sub', email: 'owner@example.test' });
  await healthContext.addCookies([{ name: 'dc2_session', value: healthCookie.split(';')[0].split('=')[1], domain: `.${BASE}`, path: '/' }]);
  const healthPage = await healthContext.newPage();
  const healthSamples = [
    { width: 390, height: 844 },
    { width: 856, height: 915 },
    { width: 959, height: 900 },
    { width: 960, height: 900 },
    { width: 961, height: 900 },
    { width: 1440, height: 1024 },
  ];
  for (const viewport of healthSamples) {
    await healthPage.setViewportSize(viewport);
    await healthPage.goto(`http://${HOST}:${port}/#/health`);
    await healthPage.waitForSelector('.health-storage-breakdown');
    await healthPage.waitForSelector('.chartbox svg.chart');
    await healthPage.screenshot({ path: path.join(OUT, `health-${viewport.width}.png`), fullPage: true });
    const layout = await healthPage.evaluate(() => {
      const summary = document.querySelector('.health-summary').getBoundingClientRect();
      const status = document.querySelector('.health-status-panel').getBoundingClientRect();
      const table = document.querySelector('.health-repository-table');
      const wrap = table.closest('.tablewrap');
      const storageItems = [...document.querySelectorAll('.health-storage-breakdown > div')].map((element) => {
        const rect = element.getBoundingClientRect();
        return { text: element.innerText, left: rect.left, right: rect.right, clipped: element.scrollWidth > element.clientWidth + 1 || element.scrollHeight > element.clientHeight + 1 };
      });
      const capacityRows = new Map();
      for (const card of document.querySelectorAll('.health-capacity-card')) {
        const rect = card.getBoundingClientRect();
        const top = Math.round(rect.top);
        if (!capacityRows.has(top)) capacityRows.set(top, []);
        capacityRows.get(top).push(rect.height);
      }
      const incidentList = document.querySelector('.health-incident-list');
      return {
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
        summaryInInitialViewport: summary.top >= 0 && status.bottom <= innerHeight,
        summaryBeforeIncidents: summary.bottom <= document.querySelector('#health-incidents-title').getBoundingClientRect().top + 1,
        capacityCards: document.querySelectorAll('.health-capacity-card').length,
        capacityRowHeightSpreads: [...capacityRows.values()].map((heights) => Math.max(...heights) - Math.min(...heights)),
        incidentAlignment: getComputedStyle(incidentList).alignItems,
        tableDisplay: getComputedStyle(table).display,
        tableScroll: wrap.scrollWidth - wrap.clientWidth,
        storageItems,
        rawStorageKeysVisible: /docker_(shared|images|build_cache|shared_volumes)/.test(document.body.innerText),
      };
    });
    const expectedTableDisplay = viewport.width <= 960 ? 'block' : 'table';
    check(`health-${viewport.width}: summary leads, aligns, and incidents keep natural height`,
      layout.summaryInInitialViewport
      && layout.summaryBeforeIncidents
      && layout.capacityCards === 4
      && layout.capacityRowHeightSpreads.every((spread) => spread <= 1)
      && layout.incidentAlignment === 'start', JSON.stringify(layout));
    check(`health-${viewport.width}: repository layout switches at 960px without horizontal overflow`,
      layout.documentOverflow <= 0 && layout.tableDisplay === expectedTableDisplay && layout.tableScroll <= 1,
      JSON.stringify({ overflow: layout.documentOverflow, tableDisplay: layout.tableDisplay, tableScroll: layout.tableScroll }));
    check(`health-${viewport.width}: all storage attribution is readable`,
      layout.storageItems.length === 5
      && layout.storageItems.every((item) => !item.clipped && item.left >= -1 && item.right <= viewport.width + 1)
      && !layout.rawStorageKeysVisible,
      JSON.stringify(layout.storageItems));
  }
  await healthContext.close();

  // Responsive global shell: the header never wraps. At and below the
  // breakpoint the same navigation links move into a keyboard-operable menu.
  const navContext = await browser.newContext({ viewport: { width: 799, height: 964 } });
  const { cookie: navCookie } = sessions.issue({ sub: 'sub', email: 'owner@example.test' });
  await navContext.addCookies([{ name: 'dc2_session', value: navCookie.split(';')[0].split('=')[1], domain: `.${BASE}`, path: '/' }]);
  const navPage = await navContext.newPage();
  await navPage.goto(`http://${HOST}:${port}/#/usage/${REPO}`);
  await navPage.waitForSelector('.usage-phase-chart');
  const compactHeader = await navPage.evaluate(() => {
    const header = document.querySelector('header.top');
    const rect = header.getBoundingClientRect();
    return { height: rect.height, overflow: document.documentElement.scrollWidth - innerWidth };
  });
  check('responsive navigation: 799px header stays on one row without document overflow',
    compactHeader.height <= 64 && compactHeader.overflow <= 0, JSON.stringify(compactHeader));
  check('responsive navigation: hamburger replaces the inline menu at 799px',
    await navPage.locator('#nav-toggle:visible').count() === 1
    && await navPage.locator('#nav:visible').count() === 0);
  await navPage.click('#nav-toggle');
  await navPage.waitForSelector('#nav:visible');
  check('interaction: hamburger opens the original navigation links in a DOM menu',
    await navPage.locator('#nav-toggle[aria-expanded="true"]').count() === 1
    && await navPage.locator('#nav a[href="#/plan"]').count() === 1
    && await navPage.locator('#nav a[href="#/health"]').count() === 1
    && await navPage.locator('#nav button').count() === 0);
  await navPage.screenshot({ path: path.join(OUT, 'navigation-799-open.png'), fullPage: true });
  await navPage.keyboard.press('Escape');
  check('interaction: Escape closes the hamburger menu and restores focus',
    await navPage.locator('#nav:visible').count() === 0
    && await navPage.locator('#nav-toggle:focus').count() === 1);
  await navPage.click('#nav-toggle');
  await navPage.click('#nav a[href="#/health"]');
  await navPage.waitForSelector('.health-summary');
  check('interaction: Console tools navigation closes the menu and opens the host destination', await navPage.locator('#nav:visible').count() === 0 && await navPage.locator('#repository-sidebar:visible').count() === 0);
  await navPage.setViewportSize({ width: 1240, height: 800 });
  await navPage.waitForFunction(() => document.querySelector('#nav-toggle')?.offsetParent !== null
    && document.querySelector('#nav')?.offsetParent === null);
  check('responsive navigation: the menu is collapsed at the declared breakpoint',
    await navPage.locator('#nav-toggle:visible').count() === 1
    && await navPage.locator('#nav:visible').count() === 0);
  await navPage.setViewportSize({ width: 1241, height: 800 });
  check('responsive navigation: Console tools stay in one compact menu on desktop', await navPage.locator('#nav-toggle:visible').count() === 1 && await navPage.locator('#nav:visible').count() === 0);
  await navContext.close();

  for (const viewport of [{ width: 1280, height: 900 }, { width: 713, height: 921 }, { width: 390, height: 844 }]) {
    for (const theme of ['light', 'dark']) {
      const context = await browser.newContext({ viewport, reducedMotion: 'reduce', colorScheme: theme });
      const { cookie } = sessions.issue({ sub: 'sub', email: 'owner@example.test' });
      await context.addCookies([{ name: 'dc2_session', value: cookie.split(';')[0].split('=')[1], domain: '.' + BASE, path: '/' }]);
      const page = await context.newPage();
      page.setDefaultTimeout(8000);
      for (const verifyJourney of [verifyWorkspace, verifyTestsDesign, verifyTestArtifacts, verifyEvidenceLayout, verifyAnnotations]) {
        try { await verifyJourney({ page, daemon, check, scenario: SCENARIOS.populated, baseUrl: `http://${HOST}:${port}/`, output: OUT, theme, viewport }); }
        catch (error) { check(`${verifyJourney.name} ${theme} ${viewport.width}`, false, error.message); }
      }
      await context.close();
    }
  }

  try { await verifyAssetRefresh({ browser, check }); }
  catch (error) { check('ordinary reload acceptance', false, error.message); }
  await browser.close();
  await edge.close();
  await daemon.close();
  report.summary = { checks: report.checks.length, failures: report.failures.length, screenshots: OUT };
  await fs.writeFile(path.join(OUT, 'report.json'), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report.summary));
  for (const f of report.failures) console.log('FAIL', f);
  process.exit(report.failures.length ? 1 : 0);
}

main().catch((error) => { console.error(error); process.exit(2); });
