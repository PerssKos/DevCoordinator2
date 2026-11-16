import assert from 'node:assert/strict';
import { execFile, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs/promises';
import https from 'node:https';
import os from 'node:os';
import path from 'node:path';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const execute = promisify(execFile);
const root = fileURLToPath(new URL('../', import.meta.url));
const tooling = process.env.CONSOLE_VERIFY_TEST_TOOLING || path.join(root, 'target/release/devcoordinator2-tooling');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const toolingStamp = execFileSync(tooling, ['--source-commit'], { encoding: 'utf8' }).trim();
const commit = /^[a-f0-9]{40}$/.test(toolingStamp) ? toolingStamp : 'a'.repeat(40);
const source = 'b'.repeat(64), config = 'c'.repeat(64);
const inventorySha = 'd'.repeat(64);
const runId = 'native-v3-isolated-proof-fixture';

async function writeJson(filename, value) {
  await fs.writeFile(filename, JSON.stringify(value), { mode: 0o600 });
  return { path: filename, sha256: sha(await fs.readFile(filename)) };
}

// The existing formal-review CLI owns validation. This is its realistic sealed
// receipt fixture, not an alternative formal or screenshot implementation.
async function formalFixture(directory, { reportPaddingBytes = 0, queuePaddingBytes = 0 } = {}) {
  const viewport = path.join(directory, 'viewport.png'), full = path.join(directory, 'full.png');
  await fs.writeFile(viewport, 'isolated viewport fixture');
  await fs.writeFile(full, 'isolated full-page fixture');
  const queue = await writeJson(path.join(directory, 'review-queue.json'), {
    schemaVersion: 1, kind: 'formal-web-ui-review-queue', runId: 'formal-fixture', entries: [{ reviewCellKey: 'cell-1' }],
    padding: 'q'.repeat(queuePaddingBytes),
  });
  const cell = {
    reviewCellKey: 'cell-1', cellId: 'cell-1', status: 'review-required', theme: 'light',
    targetName: 'isolated-proof-validator-fixture', requestedPath: '/', stateName: 'default',
    viewport: { name: 'desktop', width: 1440, height: 900 }, sourceFingerprint: 'd'.repeat(64), intentFingerprint: 'e'.repeat(64),
    screenshots: { viewport: { path: viewport, sha256: sha(await fs.readFile(viewport)) }, fullPage: { path: full, sha256: sha(await fs.readFile(full)) } },
  };
  const journey = await writeJson(path.join(directory, 'journey-evidence.json'), { kind: 'isolated-proof-validator-fixture' });
  const formal = {
    result: 'passed', runId: 'formal-fixture', freshComplete: true, exitCode: 0,
    sourceSha256: 'f'.repeat(64), sourceDigestScope: 'declared UI fingerprints, deliberately different from repository source',
    configSha256: '1'.repeat(64), verifierSha256: '2'.repeat(64), planSha256: '3'.repeat(64), candidateId: '4'.repeat(64),
    coverage: { status: 'passed', readinessEligible: true, requiredCells: 1, checkedCells: 1 },
  };
  const reportValue = {
    schemaVersion: 2, runId: 'formal-fixture', formal,
    evidence: { config: { sha256: formal.configSha256 }, verifier: { sha256: formal.verifierSha256 }, journey: { path: journey.path } },
    review: { queueSha256: queue.sha256, cells: [cell] },
    pages: [{ cellId: cell.cellId, outcome: 'checked', target: { name: cell.targetName, stateName: cell.stateName, theme: cell.theme }, viewport: cell.viewport,
      review: { sourceFingerprint: cell.sourceFingerprint, intentFingerprint: cell.intentFingerprint }, screenshots: cell.screenshots,
      sourceBinding: { status: 'matched', expected: `git:${commit}`, observed: `git:${commit}` } }],
    padding: 'r'.repeat(reportPaddingBytes),
  };
  const report = await writeJson(path.join(directory, 'report.json'), reportValue);
  const files = [];
  for (const [filename, kind] of [[report.path, 'report'], [queue.path, 'review-queue'], [journey.path, 'journey-evidence'], [viewport, 'screenshot'], [full, 'screenshot']]) {
    const bytes = await fs.readFile(filename);
    files.push({ kind, identity: sha(await fs.realpath(filename)), sha256: sha(bytes), bytes: bytes.length });
  }
  const manifest = await writeJson(path.join(directory, 'formal-artifacts.json'), { schemaVersion: 1, kind: 'formal-ui-artifact-manifest', runId: 'formal-fixture', files });
  await writeJson(path.join(directory, 'formal-receipt.json'), { formal: { ...formal, evidence: { manifestSha256: manifest.sha256 } } });
  const decisions = await writeJson(path.join(directory, 'decisions.json'), [{ reviewCellKey: 'cell-1', decision: 'pass', note: '' }]);
  const manual = path.join(directory, 'manual-review.json');
  await execute(tooling, ['formal-ui', 'review', '--report', report.path, '--queue', queue.path, '--decisions', decisions.path, '--out', manual], { timeout: 10000 });
  return { report, queue, review: { path: manual, sha256: sha(await fs.readFile(manual)) } };
}

async function fixture(t, formalOptions = {}) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-native-v3-proof-'));
  const outputs = [], requests = [];
  let server;
  t.after(async () => {
    if (server) { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
    for (const output of outputs) await fs.rm(output, { recursive: true, force: true });
    await fs.rm(directory, { recursive: true, force: true });
  });
  const key = path.join(directory, 'key.pem'), cert = path.join(directory, 'cert.pem');
  let proofTooling = tooling;
  if (!/^[a-f0-9]{40}$/.test(toolingStamp)) {
    // Debug binaries deliberately identify as "development". This test-only
    // executable supplies the fixture stamp and forwards validation to the real
    // current CLI; the production helper has no development identity bypass.
    proofTooling = path.join(directory, 'stamped-tooling-fixture.cjs');
    await fs.writeFile(proofTooling, `#!${process.execPath}\nconst{spawnSync}=require('node:child_process');if(process.argv[2]==='--source-commit'){console.log(${JSON.stringify(commit)});}else{const r=spawnSync(${JSON.stringify(tooling)},process.argv.slice(2),{stdio:'inherit'});process.exit(r.status??1);}\n`, { mode: 0o700 });
  }
  execFileSync('/usr/bin/openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', key, '-out', cert, '-days', '1', '-subj', '/CN=native-proof.fixture.test', '-addext', 'subjectAltName=DNS:native-proof.fixture.test'], { stdio: 'ignore' });
  const formal = await formalFixture(directory, formalOptions);
  const binaryHashes = Object.fromEntries(['devcoordinator2', 'devcoordinator2-tooling', 'devcoordinator2-executor', 'devcoordinator2-migrate-authority'].map((name, i) => [name, String(i + 5).repeat(64)]));
  const checked = Date.now();
  const live = await writeJson(path.join(directory, 'live.json'), { schema: 1, result: 'passed', source_commit: commit, edge_source_commit: commit, binaries: binaryHashes, checked_at_ms: checked, checks: ['browser', 'edge-source', 'critical-journeys', 'critical-api', 'mcp-reconnect'], evidence: ['private-original-not-for-copy'] });
  const finalization = await writeJson(path.join(directory, 'finalization.json'), { schema: 1, status: 'finalized', source_commit: commit, live_acceptance_sha256: live.sha256, postgres_archive_sha256: '9'.repeat(64), retired_sources: 4 });
  const backup = await writeJson(path.join(directory, 'backup.json'), { verified: true, isolated_restore_removed: true, matched_frozen_authority: false, archive_sha256: '9'.repeat(64), archive_bytes: 1234, tables: [], sequences: [] });
  const helper = await writeJson(path.join(directory, 'helper8.json'), { schema: 1, status: 'retired', inventory_sha256: inventorySha, retired_sources: 8, gates: { source_commit: commit, manifest_sha256: '5'.repeat(64), finalization_sha256: finalization.sha256, live_acceptance_sha256: live.sha256, archive_sha256: '9'.repeat(64), verified_report_sha256: backup.sha256, current_authority_sha256: '6'.repeat(64), postgres_config_sha256: '7'.repeat(64), finalized_transaction: directory } });
  const api = await writeJson(path.join(directory, 'api.json'), { schema: 1, result: 'passed', source_commit: commit, protocol_version: 3, checked_at_ms: checked, checks: 12, mutation_readbacks: 3, unexpected_errors: 0 });
  const mcp = await writeJson(path.join(directory, 'mcp.json'), { schema: 1, result: 'passed', source_commit: commit, protocol_version: 3, checked_at_ms: checked, reconnected: true, discovery: true, calls_succeeded: 3, unexpected_errors: 0 });
  const migration = await writeJson(path.join(directory, 'migration.json'), { schema: 1, result: 'passed', source_commit: commit, tables: 3, rows: 120, all_tables_equal: true, restart_equal: true, foreign_keys_valid: true, identities_valid: true, sqlite_source_sha256: '8'.repeat(64) });
  const capabilities = await writeJson(path.join(directory, 'capabilities.json'), { schema: 1, result: 'passed', source_commit: commit, baseline_operations: 193, preserved_operations: 193, current_operations: 194, baseline_catalog_sha256: '1'.repeat(64), current_catalog_sha256: '2'.repeat(64), authorization_unchanged: true, cli_matches_catalog: true, mcp_matches_catalog: true, live_discovery_verified: true });
  const completion = await writeJson(path.join(directory, 'completion.json'), { schema_version: 1, claim: 'complete', source_sha256: source, config_sha256: config,
    capabilities: [{ id: 'postgres-v3-live', scope: 'product', state: 'real_e2e', expected_result: 'Live v3 capability and source checks pass with retained final recovery evidence.', evidence_refs: ['live', 'formal', 'critical-api', 'mcp', 'migration', 'finalization', 'backup', 'helper8', 'retirement'].map(name => `run/${runId}/evidence/${name}.json`) }] });
  const acceptance = { schema: 1, target: 'postgres-v3-refactor', repository_id: 'r9999999999999999', source_commit: commit, source_sha256: source, config_sha256: config, run_id: runId, schema_version: 35,
    formal: [formal], receipts: { live, 'critical-api': api, mcp, migration, capabilities, finalization, backup, helper8: helper }, completion, helper8_inventory_sha256: inventorySha, helper8_binary_sha256: 'a'.repeat(64),
    retirement_targets: Array.from({ length: 12 }, (_, i) => ({ path: path.join(directory, `removed-${i}.sqlite3`), sha256: '8'.repeat(64), group: i < 4 ? 'finalizer' : 'helper8' })) };
  const input = path.join(directory, 'acceptance.json');
  await writeJson(input, acceptance);
  server = https.createServer({ key: await fs.readFile(key), cert: await fs.readFile(cert) }, async (req, res) => {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    requests.push({ method: req.method, path: req.url, body: Buffer.concat(chunks).toString() });
    if (req.url.startsWith('/api/v3/')) {
      const operation = req.url.slice('/api/v3/'.length);
      const allowed = new Set(['ping', 'plan.overview', 'decision.tail', 'test.admission.status', 'health.summary', 'storage.inventory']);
      res.writeHead(allowed.has(operation) ? 200 : 405, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ protocol: 3, ok: allowed.has(operation), data: operation === 'ping' ? { protocol_version: 3, schema_version: 35, source_commit: commit } : { private_record: 'raw-record-fixture-must-never-be-copied' } }));
      return;
    }
    const filename = req.url === '/' ? path.join(root, 'console/index.html') : path.join(root, 'console', decodeURIComponent(req.url.slice(1)));
    try { res.writeHead(200, { 'content-type': req.url === '/' ? 'text/html; charset=utf-8' : 'application/octet-stream' }); res.end(await fs.readFile(filename)); }
    catch { res.writeHead(404); res.end(); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  async function run(environment = {}) {
    const relative = `target/native-console-proof-fixture-${path.basename(directory)}-${outputs.length}`;
    const output = path.join(root, relative); outputs.push(output);
    let result;
    try {
      result = await execute(process.execPath, [path.join(root, 'console/verify-live-tickets.mjs')], { timeout: 20000, maxBuffer: 32768,
        env: { ...process.env, NODE_EXTRA_CA_CERTS: cert, CONSOLE_VERIFY_MODE: 'postgres-v3', CONSOLE_VERIFY_BASE_URL: `https://native-proof.fixture.test:${server.address().port}/`,
          CONSOLE_VERIFY_SOURCE_SHA256: source, CONSOLE_VERIFY_CONFIG_SHA256: config, CONSOLE_VERIFY_RUN_ID: runId, CONSOLE_VERIFY_OUT: relative,
          CONSOLE_VERIFY_ACCEPTANCE: input, CONSOLE_VERIFY_ACCEPTANCE_SHA256: sha(await fs.readFile(input)), CONSOLE_VERIFY_TOOLING: proofTooling, CONSOLE_VERIFY_PLAYWRIGHT: path.join(directory, 'must-not-launch-browser'), DEVCOORDINATOR_EVIDENCE_DIR: directory, DEVCOORDINATOR_RUN_ID: runId, ...environment } });
      result.code = 0;
    } catch (error) { result = { code: error.code, stdout: error.stdout, stderr: error.stderr }; }
    return { ...result, output };
  }
  const replaceReceipt = async (name, change) => {
    const ref = acceptance.receipts[name], value = JSON.parse(await fs.readFile(ref.path));
    change(value); acceptance.receipts[name] = await writeJson(ref.path, value);
    await writeJson(input, acceptance);
  };
  return { directory, input, acceptance, requests, run, replaceReceipt, rewrite: () => writeJson(input, acceptance) };
}

test('explicit v3 proof binds real validator, native read-only traffic, complete inventory and all twelve absences', { timeout: 30000 }, async t => {
  const f = await fixture(t);
  f.acceptance.receipts['critical-api'].original_sha256 = '0'.repeat(64);
  f.acceptance.receipts['critical-api'].original_bytes = 98765;
  await f.rewrite();
  for (const ref of [f.acceptance.completion, ...Object.values(f.acceptance.receipts), ...f.acceptance.formal.flatMap(batch => Object.values(batch))]) await fs.chmod(ref.path, 0o440);
  const result = await f.run();
  assert.equal(result.code, 0, result.stderr);
  const proof = JSON.parse(await fs.readFile(path.join(result.output, 'delivery.json')));
  assert.equal(proof.target, 'postgres-v3-refactor');
  assert.equal(proof.source_sha256, source);
  assert.equal(proof.native_console.daemon_source_commit, commit);
  const completion = JSON.parse(await fs.readFile(path.join(result.output, 'completion.json')));
  assert.equal(completion.claim, 'complete');
  assert.equal(completion.config_sha256, config);
  assert.deepEqual(f.requests.filter(req => req.path.startsWith('/api/v3/')).map(req => req.path.slice(8)).sort(), ['decision.tail', 'health.summary', 'ping', 'ping', 'plan.overview', 'storage.inventory', 'test.admission.status']);
  for (const operation of ['plan.overview', 'decision.tail', 'storage.inventory']) {
    const request = f.requests.find(req => req.path === `/api/v3/${operation}`), params = JSON.parse(request.body);
    assert.equal(params.repository_id, f.acceptance.repository_id);
    assert.equal(params.path, undefined);
  }
  const files = await fs.readdir(path.join(result.output, 'evidence'));
  const retained = (await Promise.all(files.map(name => fs.readFile(path.join(result.output, 'evidence', name), 'utf8')))).join('\n');
  assert.ok(!retained.includes('raw-record-fixture-must-never-be-copied'));
  assert.ok(!retained.includes('private-original-not-for-copy'));
  assert.ok(!retained.includes(f.directory));
  const index = JSON.parse(await fs.readFile(path.join(result.output, 'evidence-index.json')));
  assert.equal(index.inputs.find(row => row.id === 'critical-api').original_sha256, '0'.repeat(64));
  assert.equal(index.inputs.find(row => row.id === 'critical-api').original_bytes, 98765);
  assert.notEqual(index.governed_source_sha256, JSON.parse(await fs.readFile(path.join(result.output, 'evidence/formal.json'))).batches[0].formal_ui_source_sha256);
});

test('accepts complete production-sized formal report and queue artifacts within their bounded caps', { timeout: 60000 }, async t => {
  const f = await fixture(t, { reportPaddingBytes: 17 * 1024 * 1024, queuePaddingBytes: 6 * 1024 * 1024 });
  const result = await f.run();
  assert.equal(result.code, 0, result.stderr);
  assert.equal(f.requests.filter(request => request.path.startsWith('/api/v3/')).length, 7);
  const reportBytes = (await fs.stat(f.acceptance.formal[0].report.path)).size;
  const queueBytes = (await fs.stat(f.acceptance.formal[0].queue.path)).size;
  assert.ok(reportBytes > 16 * 1024 * 1024 && reportBytes <= 32 * 1024 * 1024);
  assert.ok(queueBytes > 2 * 1024 * 1024 && queueBytes <= 8 * 1024 * 1024);
});

test('refuses a formal report beyond the bounded production parser cap', { timeout: 60000 }, async t => {
  const f = await fixture(t, { reportPaddingBytes: 32 * 1024 * 1024 + 1 });
  const result = await f.run();
  assert.notEqual(result.code, 0);
  assert.equal(JSON.parse(result.stderr).stage, 'formal-report-0');
  assert.equal(f.requests.length, 0);
  await assert.rejects(fs.stat(path.join(result.output, 'delivery.json')), { code: 'ENOENT' });
});

test('refuses a review queue beyond the bounded production parser cap', { timeout: 60000 }, async t => {
  const f = await fixture(t, { queuePaddingBytes: 8 * 1024 * 1024 + 1 });
  const result = await f.run();
  assert.notEqual(result.code, 0);
  assert.equal(JSON.parse(result.stderr).stage, 'formal-queue-0');
  assert.equal(f.requests.length, 0);
  await assert.rejects(fs.stat(path.join(result.output, 'delivery.json')), { code: 'ENOENT' });
});

test('missing, stale or incomplete final gates refuse proof before native requests', { timeout: 30000 }, async t => {
  const cases = [
    ['ticket target relabel', f => { f.acceptance.target = 'upstream-feature-requests'; return f.rewrite(); }, 'acceptance-binding'],
    ['unfinished finalization', f => f.replaceReceipt('finalization', v => { v.status = 'retiring'; }), 'finalization'],
    ['unremoved restore database', f => f.replaceReceipt('backup', v => { v.isolated_restore_removed = false; }), 'backup'],
    ['unreviewed helper inventory', f => f.replaceReceipt('helper8', v => { v.inventory_sha256 = '0'.repeat(64); }), 'helper8'],
    ['protocol2 API support', f => f.replaceReceipt('critical-api', v => { v.protocol_version = 2; }), 'critical-api'],
    ['MCP was never reconnected', f => f.replaceReceipt('mcp', v => { v.reconnected = false; }), 'mcp'],
    ['missing preserved catalog operation', f => f.replaceReceipt('capabilities', v => { v.preserved_operations = 192; }), 'capabilities'],
    ['remaining exact migration source', async f => { await fs.writeFile(f.acceptance.retirement_targets[0].path, 'preserve'); }, 'retirement-absence'],
    ['symlink ancestor must not prove absence', async f => { const link = path.join(f.directory, 'redirect'); await fs.symlink(f.directory, link); f.acceptance.retirement_targets[0].path = path.join(link, 'missing.sqlite3'); await f.rewrite(); }, 'retirement-absence'],
    ['fixture-only product capability', async f => { const ref = f.acceptance.completion, value = JSON.parse(await fs.readFile(ref.path)); value.capabilities[0].state = 'fixture_only'; f.acceptance.completion = await writeJson(ref.path, value); await f.rewrite(); }, 'completion'],
    ['tampered formal screenshot', async f => { await fs.writeFile(path.join(f.directory, 'viewport.png'), 'changed'); }, 'formal-manual'],
    ['unsealed caller copy', async f => { await fs.chmod(f.acceptance.receipts.helper8.path, 0o644); }, 'helper8'],
    ['unknown completion properties are not copied', async f => { const ref = f.acceptance.completion, value = JSON.parse(await fs.readFile(ref.path)); value.raw_records = 'must-not-copy'; f.acceptance.completion = await writeJson(ref.path, value); await f.rewrite(); }, 'completion'],
    ['ticket-only inventory cannot omit v3 boundaries', async f => { const ref = f.acceptance.completion, value = JSON.parse(await fs.readFile(ref.path)); value.capabilities[0].evidence_refs = [`run/${runId}/evidence/critical-api.json`]; f.acceptance.completion = await writeJson(ref.path, value); await f.rewrite(); }, 'completion'],
  ];
  for (const [name, change, stage] of cases) await t.test(name, async t => {
    const f = await fixture(t); await change(f); const result = await f.run();
    assert.notEqual(result.code, 0);
    assert.equal(JSON.parse(result.stderr).stage, stage);
    assert.equal(f.requests.length, 0);
    await assert.rejects(fs.stat(path.join(result.output, 'delivery.json')), { code: 'ENOENT' });
  });
});

test('the executor plan binds the exact reviewed acceptance input bytes', { timeout: 30000 }, async t => {
  const f = await fixture(t), result = await f.run({ CONSOLE_VERIFY_ACCEPTANCE_SHA256: '0'.repeat(64) });
  assert.notEqual(result.code, 0);
  assert.equal(JSON.parse(result.stderr).stage, 'acceptance-input');
  assert.equal(f.requests.length, 0);
});
