import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createRequire } from 'node:module';
import { createEdge } from '../edge/devcoordinator2-edge.mjs';
import { createSessionManager } from '../edge/lib/session.mjs';
import { canonicalJson } from '../edge/lib/routes-store.mjs';

const root = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..');
const out = path.resolve(process.env.GLOSSARY_VERIFY_OUT || path.join(os.tmpdir(), 'dc2-glossary-verification'));
const runtime = process.env.CONSOLE_VERIFY_PLAYWRIGHT || '/opt/holyskills-validation-runtime';
const { chromium } = createRequire(path.join(runtime, 'package.json'))('playwright');
const execute = promisify(execFile);
const project = 'r1111111111111111';
const otherProject = 'r2222222222222222';
const checks = [];
const pageErrors = [];
let sequence = 0;
let primaryId;
let relatedId;
let browser;
let edge;
let socketServer;
let bridge;
let temporary;
const pending = new Map();

function request(operation, params = {}, identity = 'owner@example.test') {
  const id = `glossary-${++sequence}`;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    bridge.stdin.write(`${JSON.stringify({ protocol: 2, id, operation, params, client: { kind: identity ? 'edge' : 'other', ...(identity ? { identity } : {}) } })}\n`);
  });
}

async function call(operation, params = {}, identity) {
  const result = await request(operation, params, identity);
  assert.equal(result.ok, true, `${operation}: ${result.error?.message}`);
  return result.data;
}

function fixtureConcept(name, rule = 'default') {
  return { name, definition: 'One execution of a declared test.', context: '', rule, status: 'approved', languages: { en: { preferred: name, allowed: [], deprecated: [], usage: '', examples: [], reviewed: true } }, related: [], specialization_reason: '' };
}

async function check(name, run, page) {
  const started = Date.now();
  try { await run(); checks.push({ name, status: 'passed', duration_ms: Date.now() - started }); }
  catch (error) {
    const filename = `${checks.length}-${name.replace(/[^a-z0-9]+/gi, '-')}.log`;
    await fs.writeFile(path.join(out, filename), error.stack || String(error));
    if (page && !page.isClosed()) await page.screenshot({ path: path.join(out, `${filename}.png`), fullPage: true }).catch(() => {});
    checks.push({ name, status: 'failed', duration_ms: Date.now() - started, evidence: filename });
  }
}

async function main() {
  await fs.mkdir(out, { recursive: true });
  temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-glossary-browser-'));
  const fixture = process.env.GLOSSARY_FIXTURE_BINARY || path.join(root, 'target/debug/examples/glossary_console_fixture');
  bridge = spawn(fixture, [path.join(temporary, 'authority')], { stdio: ['pipe', 'pipe', 'pipe'] });
  bridge.stderr.on('data', (bytes) => fs.appendFile(path.join(out, 'backend.stderr.log'), bytes));
  bridge.on('exit', (code) => { for (const callback of pending.values()) callback.reject(new Error(`Fixture exited with ${code}`)); pending.clear(); });
  readline.createInterface({ input: bridge.stdout }).on('line', (line) => {
    const result = JSON.parse(line); const callback = pending.get(result.id);
    pending.delete(result.id); callback?.resolve(result);
  });
  await call('ping');
  const socketPath = path.join(temporary, 'bridge.sock');
  socketServer = net.createServer({ allowHalfOpen: true }, (socket) => {
    let buffer = '';
    socket.on('data', (bytes) => {
      buffer += bytes.toString();
      if (!buffer.endsWith('\n')) return;
      const input = JSON.parse(buffer); buffer = '';
      request(input.operation, input.params, input.client.identity || null).then((result) => socket.end(`${JSON.stringify({ ...result, id: input.id })}\n`), () => socket.destroy());
    });
    socket.on('error', () => {});
  });
  await new Promise((resolve, reject) => { socketServer.once('error', reject); socketServer.listen(socketPath, resolve); });
  const payload = { generation: 1, published_at: '2026-09-05T12:00:00Z', domain: 'example.test', routes: [], access: { owners: ['owner@example.test'], grants: [] } };
  await fs.writeFile(path.join(temporary, 'routes.json'), JSON.stringify({ schema: 1, payload_sha256: crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex'), ...payload }));
  const secret = crypto.randomBytes(32).toString('hex');
  const sessions = createSessionManager({ secret, ttlMs: 3600000, cookieName: 'dc2_session', secure: false });
  edge = await createEdge({ baseDomain: 'example.test', consoleHost: '127.0.0.1', httpPort: 0, httpOnly: true, sessionSecret: secret, oidcIssuer: 'http://127.0.0.1:1', oidcClientId: '', oidcClientSecret: '', routesFile: path.join(temporary, 'routes.json'), stateDir: path.join(temporary, 'edge'), daemonSocket: socketPath, consoleDir: path.join(root, 'console') }, { log: { info() {}, warn() {}, error() {}, debug() {} } });
  const [port] = await edge.listen();
  const base = `http://127.0.0.1:${port}/`;
  browser = await chromium.launch();
  const cookieFor = (email) => {
    const { cookie } = sessions.issue({ sub: email, email });
    return { name: 'dc2_session', value: cookie.split(';')[0].slice('dc2_session='.length), url: base };
  };
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  await context.addCookies([cookieFor('owner@example.test')]);
  const page = await context.newPage();
  page.on('pageerror', (error) => pageErrors.push(error.message));
  if (process.env.CONSOLE_VERIFY_PRESENTATION_ONLY) {
    await check('Repository appearance saves through the rendered Console into the real database', async () => {
      await page.goto(`${base}#/plan/${project}`);
      await page.locator('#repository-presentation').click();
      await page.locator('#repository-presentation-dialog input[name=display_name]').fill('Fieldwork');
      await page.locator('#repository-presentation-dialog input[value=plane]').check();
      await page.locator('#repository-presentation-dialog button[type=submit]').click();
      await page.waitForFunction(() => document.querySelector('#workspace-heading')?.textContent === 'Fieldwork');
      const collection = await call('plan.overview');
      const saved = collection.repositories.find((repository) => repository.repository_id === project);
      assert.equal(saved.display_name, 'Vocabulary project');
      assert.equal(saved.presentation.display_name, 'Fieldwork');
      assert.equal(saved.presentation.icon, 'plane');
      assert.equal(collection.repositories.find((repository) => repository.repository_id === otherProject).presentation, undefined);
      await page.reload();
      await page.waitForFunction(() => document.querySelector('#workspace-heading')?.textContent === 'Fieldwork');
      assert.equal(await page.locator('.workspace-repository[aria-current]').getAttribute('data-repository-icon'), 'plane');
    }, page);
    await check('Repository appearance preserves filtered reads and administrator write permissions', async () => {
      const collection = await call('plan.overview', {}, 'viewer@example.test');
      assert.deepEqual(collection.repositories.map((repository) => repository.repository_id), [project]);
      assert.equal(collection.repositories.find((repository) => repository.repository_id === project).presentation.display_name, 'Fieldwork');
      const denied = await request('repository.presentation.update', { repository_id: project, display_name: 'Unauthorized', icon: 'code' }, 'viewer@example.test');
      assert.equal(denied.ok, false); assert.equal(denied.error.code, 'permission_denied');
      const viewer = await browser.newContext({ viewport: { width: 927, height: 873 } });
      try {
        await viewer.addCookies([cookieFor('viewer@example.test')]);
        const viewerPage = await viewer.newPage();
        await viewerPage.goto(`${base}#/plan/${project}`);
        await viewerPage.locator('#workspace-heading').filter({ hasText: 'Fieldwork' }).waitFor();
        assert.equal(await viewerPage.locator('#repository-presentation').isVisible(), false);
      } finally { await viewer.close(); }
    }, page);
    await check('Repository appearance validation cannot change saved data', async () => {
      for (const params of [
        { display_name: '   ', icon: 'code' },
        { display_name: 'Invalid\nname', icon: 'code' },
        { display_name: 'Fieldwork', icon: '../private' },
      ]) {
        const rejected = await request('repository.presentation.update', { repository_id: project, ...params });
        assert.equal(rejected.ok, false);
      }
      const collection = await call('plan.overview');
      assert.equal(collection.repositories.find((repository) => repository.repository_id === project).presentation.display_name, 'Fieldwork');
    }, page);
    await check('Default appearance is restored through the UI and remains restored after reload', async () => {
      await page.locator('#repository-presentation').click();
      await page.locator('[data-presentation-reset]').click();
      await page.locator('#repository-presentation-dialog button[type=submit]').click();
      await page.waitForFunction(() => document.querySelector('#workspace-heading')?.textContent === 'Vocabulary project');
      await page.reload();
      await page.waitForFunction(() => document.querySelector('#workspace-heading')?.textContent === 'Vocabulary project');
      const collection = await call('plan.overview');
      assert.equal(collection.repositories.find((repository) => repository.repository_id === project).presentation, undefined);
      assert.deepEqual(pageErrors, []);
    }, page);
    await fs.writeFile(path.join(out, 'report.json'), JSON.stringify({ checks }, null, 2));
    const failures = checks.filter((item) => item.status === 'failed');
    console.log(JSON.stringify({ checks: checks.length, failures, report: path.join(out, 'report.json') }));
    process.exitCode = failures.length ? 1 : 0;
    await context.close(); return;
  }
  const go = async (scope = 'shared', identity = '') => {
    await page.goto(`${base}?journey=${++sequence}#/glossary/${scope}${identity ? `/${identity}` : ''}`);
    await page.locator(identity ? '.glossary-detail' : '#glossary-concepts').waitFor();
  };
  const detail = () => call('glossary.get', { concept_id: primaryId });

  await check('Empty collection leads and add editor focuses immediately', async () => {
    await go();
    assert.match(await page.locator('#glossary-concepts').innerText(), /No concepts yet/);
    await page.getByRole('button', { name: 'Add concept', exact: true }).click();
    assert.equal(await page.locator('dialog [name=name]').evaluate((element) => element === document.activeElement), true);
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    assert.equal(await page.getByRole('button', { name: 'Add concept', exact: true }).evaluate((element) => element === document.activeElement), true);
  }, page);

  await check('Create multilingual concept through UI and prove database persistence', async () => {
    await go(); await page.getByRole('button', { name: 'Add concept', exact: true }).click();
    await page.locator('dialog [name=name]').fill('Test execution');
    await page.locator('dialog [name=definition]').fill('One execution of a declared test.');
    await page.locator('dialog [name=status]').selectOption('approved');
    for (const [language, preferred] of [['en', 'Test run'], ['ru', 'Запуск теста']]) {
      await page.locator('#glossary-new-language').fill(language);
      await page.getByRole('button', { name: 'Add language', exact: true }).click();
      const row = page.locator(`[data-language=${language}]`);
      await row.locator('[name=preferred]').fill(preferred);
      if (language === 'en') { await row.locator('[name=allowed]').fill('Test runs'); await row.locator('[name=deprecated]').fill('Job'); }
      await row.locator('[name=reviewed]').check();
    }
    await page.getByRole('button', { name: 'Save concept', exact: true }).click();
    await page.locator('.glossary-card a').filter({ hasText: 'Test execution' }).waitFor();
    primaryId = (await call('glossary.list', { query: 'Запуск теста' })).entries[0].concept_id;
    assert.equal((await detail()).entry.concept.languages.ru.preferred, 'Запуск теста');
    await page.reload(); await page.locator('#glossary-concepts').waitFor();
    await page.locator('.glossary-card a').filter({ hasText: 'Test execution' }).click();
    await page.locator('.glossary-detail').waitFor();
    assert.match(await page.locator('.glossary-language-grid').innerText(), /Запуск теста/);
  }, page);

  if (!primaryId) primaryId = (await call('glossary.save', { expected_revision: (await call('glossary.list')).profile.revision, concept: fixtureConcept('Test execution') })).concept_id;

  await check('Cancel preserves stored text and validation preserves the draft', async () => {
    await go('shared', primaryId); await page.getByRole('button', { name: 'Edit concept', exact: true }).click();
    await page.locator('dialog [name=name]').fill('Discard this name');
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    assert.equal((await detail()).entry.concept.name, 'Test execution');
    await page.getByRole('button', { name: 'Edit concept', exact: true }).click();
    await page.locator('[data-language=en] [name=deprecated]').fill((await detail()).entry.concept.languages.en.preferred);
    await page.getByRole('button', { name: 'Save concept', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: 'must not overlap' }).waitFor();
    assert.equal(await page.locator('dialog [name=name]').inputValue(), 'Test execution');
    await page.locator('[data-language=en] [name=deprecated]').fill('Job');
    await page.getByRole('button', { name: 'Save concept', exact: true }).click();
    await page.locator('dialog').waitFor({ state: 'detached' });
  }, page);

  await check('Concurrent edits reject stale writes and offer explicit recovery', async () => {
    await go('shared', primaryId); await page.getByRole('button', { name: 'Edit concept', exact: true }).click();
    const current = await detail();
    await call('glossary.save', { expected_revision: current.profile.revision, concept_id: primaryId, concept: { ...current.entry.concept, context: 'Changed by another editor.' } });
    await page.locator('dialog [name=name]').fill('Stale draft');
    await page.getByRole('button', { name: 'Save concept', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: 'changed from revision' }).waitFor();
    assert.equal((await detail()).entry.concept.name, 'Test execution');
    await page.getByRole('button', { name: 'Reload latest and discard this draft', exact: true }).click();
    await page.waitForFunction(() => document.querySelector('dialog [name=context]')?.value === 'Changed by another editor.');
    assert.equal(await page.locator('dialog [name=name]').inputValue(), 'Test execution');
    await page.locator('[data-language=en] [name=reviewed]').check();
    await page.getByRole('button', { name: 'Save concept', exact: true }).click();
    await page.locator('dialog').waitFor({ state: 'detached' });
  }, page);

  await check('Related concepts can be found linked followed and removed', async () => {
    const current = await detail();
    relatedId = (await call('glossary.save', { expected_revision: current.profile.revision, concept: fixtureConcept('Test definition') })).concept_id;
    await go('shared', primaryId); await page.getByRole('button', { name: 'Edit concept', exact: true }).click();
    await page.locator('dialog summary').filter({ hasText: 'Related concepts' }).click();
    await page.locator('#glossary-related-query').fill('Test definition');
    await page.getByRole('button', { name: 'Find concepts', exact: true }).click();
    await page.getByRole('button', { name: 'Add Test definition', exact: true }).click();
    await page.getByRole('button', { name: 'Save concept', exact: true }).click();
    await page.locator('#glossary-related a').filter({ hasText: 'Test definition' }).click();
    await page.locator('.glossary-detail h2').filter({ hasText: 'Test definition' }).waitFor();
  }, page);

  await check('Project adoption specialization conflict and return to shared are real', async () => {
    await go(project); await page.getByRole('button', { name: 'Review shared update', exact: true }).click();
    await page.getByRole('button', { name: /^Adopt revision/ }).click(); await page.locator('dialog').waitFor({ state: 'detached' });
    await go(project, primaryId); await page.getByRole('button', { name: 'Specialize for project', exact: true }).click();
    await page.locator('dialog [name=specialization_reason]').fill('The project groups executions for its users.');
    await page.locator('dialog [name=name]').fill('Project execution');
    await page.getByRole('button', { name: 'Save concept', exact: true }).click(); await page.locator('dialog').waitFor({ state: 'detached' });
    assert.equal((await call('glossary.get', { repository_id: project, concept_id: primaryId })).entry.origin, 'specialized');
    const current = await detail();
    await call('glossary.save', { expected_revision: current.profile.revision, concept_id: primaryId, concept: { ...current.entry.concept, rule: 'mandatory' } });
    await go(project); await page.getByRole('button', { name: 'Review shared update', exact: true }).click();
    await page.getByRole('button', { name: /^Adopt revision/ }).click();
    await page.getByRole('alert').filter({ hasText: 'conflicts with a project specialization' }).waitFor();
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    await go(project, primaryId); await page.getByRole('button', { name: 'Use shared concept', exact: true }).click();
    await page.getByRole('button', { name: 'Specialize for project', exact: true }).waitFor();
    await go(project); await page.getByRole('button', { name: 'Review shared update', exact: true }).click();
    await page.getByRole('button', { name: /^Adopt revision/ }).click(); await page.locator('dialog').waitFor({ state: 'detached' });
    await go(project, primaryId);
    assert.equal(await page.getByRole('button', { name: 'Edit concept', exact: true }).count(), 0);
    assert.equal(await page.getByRole('button', { name: 'Specialize for project', exact: true }).count(), 0);
    await page.getByRole('link', { name: /^Shared source at revision/ }).click();
    await page.getByRole('link', { name: 'Return to the current glossary', exact: true }).waitFor();
  }, page);

  await check('Guidance languages history and project adoption are navigable', async () => {
    await go(); await page.getByRole('button', { name: 'Guidance and languages', exact: true }).click();
    await page.locator('dialog [name=languages]').fill('en, ru, ar');
    await page.getByRole('button', { name: 'Add guideline', exact: true }).click();
    await page.locator('dialog [name=key]').fill('Truthful status');
    await page.locator('dialog [name=text]').fill('Use names that describe the observed result.');
    await page.locator('dialog [name=rule]').selectOption('mandatory');
    await page.getByRole('button', { name: 'Save guidance', exact: true }).click(); await page.locator('dialog').waitFor({ state: 'detached' });
    assert.equal((await call('glossary.resolve')).profile.guidelines[0].guideline.key, 'Truthful status');
    await page.getByRole('button', { name: 'History', exact: true }).click();
    await page.getByRole('link', { name: 'View this revision', exact: true }).first().click();
    await page.getByRole('link', { name: 'Return to the current glossary', exact: true }).click();
    await page.getByRole('button', { name: 'Project adoption', exact: true }).click();
    await page.locator('dialog a').filter({ hasText: 'Vocabulary project' }).click();
    await page.locator('#glossary-concepts').waitFor();
    assert.match(page.url(), new RegExp(project));
  }, page);

  await check('CLI and MCP resolve the exact Console glossary revision', async () => {
    const environment = { ...process.env, DEVCOORDINATOR2_SOCKET: socketPath, DEVCOORDINATOR2_INSTANCE_ENV: path.join(temporary, 'absent-instance.env') };
    const binary = process.env.GLOSSARY_CLI_BINARY || path.join(root, 'target/debug/devcoordinator2');
    const { stdout } = await execute(binary, ['glossary', 'resolve', '--query', 'Test execution'], { env: environment });
    const result = JSON.parse(stdout);
    assert.equal(result.ok, true);
    assert.equal(result.data.profile.revision, (await detail()).profile.revision);
    assert.equal(result.data.entries[0].concept_id, primaryId);
    const mcp = spawn(binary, ['mcp'], { env: environment, stdio: ['pipe', 'pipe', 'pipe'] });
    const callbacks = new Map();
    const lines = readline.createInterface({ input: mcp.stdout });
    lines.on('line', (line) => { const message = JSON.parse(line); if (message.id) { callbacks.get(message.id)?.(message); callbacks.delete(message.id); } });
    const send = (id, method, params) => new Promise((resolve, reject) => {
      const deadline = AbortSignal.timeout(10000);
      const expired = () => { callbacks.delete(id); reject(new Error(`MCP ${method} did not complete`)); };
      deadline.addEventListener('abort', expired, { once: true });
      callbacks.set(id, (result) => { deadline.removeEventListener('abort', expired); resolve(result); });
      mcp.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
    });
    try {
      const initialized = await send(1, 'initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'glossary-test', version: '1' } });
      assert.ok(initialized.result);
      mcp.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' })}\n`);
      const tools = await send(2, 'tools/list', {});
      assert.ok(tools.result.tools.some((tool) => tool.name === 'glossary_resolve'));
      const resolved = await send(3, 'tools/call', { name: 'glossary_resolve', arguments: { query: 'Test execution' } });
      assert.equal(resolved.result.isError || false, false);
      assert.match(JSON.stringify(resolved.result), new RegExp(primaryId));
    } finally { mcp.stdin.end(); mcp.kill('SIGTERM'); if (mcp.exitCode == null && mcp.signalCode == null) await new Promise((resolve) => mcp.once('exit', resolve)); lines.close(); }
  }, page);

  await check('Read-only users see authorized glossary scopes but cannot edit or enumerate other projects', async () => {
    const viewer = await browser.newContext(); await viewer.addCookies([cookieFor('viewer@example.test')]);
    const viewerPage = await viewer.newPage();
    try {
      await viewerPage.goto(`${base}#/glossary/shared`); await viewerPage.locator('#glossary-concepts').waitFor();
      assert.equal(await viewerPage.getByRole('button', { name: 'Add concept', exact: true }).count(), 0);
      assert.equal((await request('glossary.save', { expected_revision: 0, concept: fixtureConcept('Forbidden') }, 'viewer@example.test')).error.code, 'permission_denied');
      assert.equal((await request('glossary.impact', {}, 'viewer@example.test')).error.code, 'permission_denied');
      await viewerPage.goto(`${base}#/glossary/${project}`); await viewerPage.locator('#glossary-concepts').waitFor();
      await viewerPage.goto(`${base}#/glossary/${otherProject}`); await viewerPage.locator('.notice.denied').waitFor();
      assert.equal((await request('glossary.list', {}, 'unknown@example.test')).error.code, 'permission_denied');
    } finally { await viewer.close(); }
  }, page);

  await check('Service failure stays honest and retry restores the collection', async () => {
    await page.route('**/api/v2/glossary.list', (route) => route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ ok: false, error: { code: 'unavailable', message: 'Fixture failure' } }) }));
    await page.goto(`${base}#/glossary/shared`); await page.getByRole('button', { name: 'Retry', exact: true }).waitFor();
    await page.unroute('**/api/v2/glossary.list'); await page.getByRole('button', { name: 'Retry', exact: true }).click(); await page.locator('#glossary-concepts').waitFor();
  }, page);

  await check('Long collections pagination searching and creation remain usable', async () => {
    let revision = (await call('glossary.list')).profile.revision;
    for (let index = 0; index < 28; index += 1) revision = (await call('glossary.save', { expected_revision: revision, concept: fixtureConcept(`Reference concept ${String(index).padStart(2, '0')}`) })).revision;
    await go(); await page.getByRole('button', { name: 'More concepts', exact: true }).click(); await page.getByRole('button', { name: 'Previous concepts', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Previous concepts', exact: true }).click();
    await page.getByRole('button', { name: 'Previous concepts', exact: true }).waitFor({ state: 'detached' });
    await page.locator('#glossary-concepts').waitFor();
    await page.locator('.glossary-tools').scrollIntoViewIfNeeded();
    await page.getByRole('button', { name: 'Add concept', exact: true }).click();
    const box = await page.locator('dialog').boundingBox(); assert.ok(box.y >= 0 && box.y < 900);
    await page.keyboard.press('Escape');
    await page.locator('#glossary-search [name=query]').fill('Запуск теста'); await page.getByRole('button', { name: 'Search', exact: true }).click();
    await page.locator('.glossary-card h3').filter({ hasText: 'Test execution' }).waitFor();
    await page.getByRole('button', { name: 'Clear filters', exact: true }).click();
  }, page);

  await check('Language and related concept removal persist and filters use all equivalents', async () => {
    await go('shared', primaryId); await page.getByRole('button', { name: 'Edit concept', exact: true }).click();
    await page.locator('#glossary-new-language').fill('fr'); await page.getByRole('button', { name: 'Add language', exact: true }).click();
    await page.locator('[data-language=fr] [name=preferred]').fill('Exécution de test');
    await page.locator('[data-language=fr] [name=reviewed]').check();
    await page.locator('[data-language=fr] [name=usage]').fill('Terminologie de vérification');
    assert.equal(await page.locator('[data-language=fr] [name=reviewed]').isChecked(), false);
    await page.getByRole('button', { name: 'Remove fr language', exact: true }).click();
    await page.locator('.glossary-related-editor summary').click();
    await page.getByRole('button', { name: 'Remove related concept Test definition', exact: true }).click();
    await page.getByRole('button', { name: 'Save concept', exact: true }).click();
    await page.locator('dialog').waitFor({ state: 'detached' });
    const saved = await detail(); assert.equal(saved.entry.concept.languages.fr, undefined); assert.deepEqual(saved.entry.concept.related, []);
    await go();
    await page.locator('#glossary-search [name=language]').selectOption('ru');
    await page.locator('#glossary-search [name=status]').selectOption('approved');
    await page.locator('#glossary-search [name=origin]').selectOption('shared');
    await page.getByRole('button', { name: 'Search', exact: true }).click();
    await page.getByRole('button', { name: 'Clear filters', exact: true }).waitFor();
    assert.equal(await page.locator('.glossary-card').count(), 1);
  }, page);

  await check('Shared repository navigation and guidance removal save real project state', async () => {
    await go(); await page.locator('#nav-toggle').click();
    await page.locator('#nav a[href="#/plan"]').click();
    await page.locator(`#repository-list a[href="#/plan/${project}"]`).click();
    await page.locator(`#workspace-aspects a[href="#/glossary/${project}"]`).click();
    await page.locator('#glossary-adopt').waitFor();
    assert.equal(new URL(page.url()).hash, `#/glossary/${project}`);
    assert.equal(await page.locator('[data-project-picker]').count(), 0);
    await page.locator('main h1 a').click();
    assert.equal(new URL(page.url()).hash, `#/glossary/${project}`);
    await page.getByRole('button', { name: 'Guidance and languages', exact: true }).click();
    await page.getByRole('button', { name: 'Add guideline', exact: true }).click();
    await page.locator('.glossary-guideline-editor [name=key]').fill('Temporary guideline');
    await page.locator('.glossary-guideline-editor [name=text]').fill('Draft guidance to remove before saving.');
    await page.getByRole('button', { name: 'Remove guideline', exact: true }).click();
    await page.getByRole('button', { name: 'Save guidance', exact: true }).click();
    await page.locator('dialog').waitFor({ state: 'detached' });
    assert.deepEqual((await call('glossary.list', { repository_id: project })).profile.local_guidelines, []);
    await go(); await page.getByRole('button', { name: 'History', exact: true }).click();
    await page.getByRole('button', { name: 'Earlier revisions', exact: true }).click();
    await page.waitForFunction(() => document.querySelectorAll('.glossary-history-row').length > 10);
    await page.getByRole('button', { name: 'Close Glossary history', exact: true }).click();
  }, page);

  for (const theme of ['light', 'dark']) for (const viewport of [{ width: 1440, height: 900 }, { width: 390, height: 844 }]) {
    await check(`${theme} ${viewport.width} collection detail and editor fit`, async () => {
      await page.setViewportSize(viewport); await go();
      if (await page.locator('html').getAttribute('data-theme') !== theme) await page.locator('#theme-toggle').click();
      for (const surface of ['collection', 'detail', 'editor']) {
        if (surface === 'detail') await go('shared', primaryId);
        if (surface === 'editor') await page.getByRole('button', { name: 'Edit concept', exact: true }).click();
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), true);
        const filename = `${theme}-${viewport.width}-${surface}`;
        await page.screenshot({ path: path.join(out, `${filename}-viewport.png`) });
        await page.screenshot({ path: path.join(out, `${filename}-full.png`), fullPage: true });
        if (surface === 'editor') {
          const controls = page.locator('dialog input, dialog textarea, dialog select, dialog button');
          for (const control of await controls.all()) {
            if (!await control.isVisible()) continue;
            await control.focus();
            await control.click({ trial: true });
            const rect = await control.boundingBox();
            assert.ok(rect && rect.x >= 0 && rect.y >= 0 && rect.x + rect.width <= viewport.width + 1 && rect.y + rect.height <= viewport.height + 1);
          }
          await page.screenshot({ path: path.join(out, `${filename}-last-controls.png`) });
        }
      }
      await page.keyboard.press('Escape');
    }, page);
  }

  await check('No uncaught browser errors', async () => assert.deepEqual(pageErrors, []), page);
  await fs.writeFile(path.join(out, 'journeys.json'), JSON.stringify({ checks }, null, 2));
  if (process.env.GLOSSARY_FORMAL_BINARY) for (const theme of ['light', 'dark']) {
    await check(`Formal ${theme} rendered journey matrix`, async () => {
      const targets = [{ name: `glossary-${theme}`, url: `${base}#/glossary/shared`, theme,
        journeys: [{ id: 'browse', name: 'Browse terminology', frequencyPercent: 90, risk: 'normal' }, { id: 'edit', name: 'Edit terminology', frequencyPercent: 10, risk: 'normal' }], primaryJourney: 'browse',
        regions: [{ name: 'Concept collection', selector: '#glossary-concepts', role: 'primary-content', journey: 'browse' }],
        reviewInputs: [{ path: 'console/glossary.js', kind: 'ui-code' }, { path: 'console/glossary.css', kind: 'style' }, { path: 'console/index.html', kind: 'ui-code' }, { path: 'console/design-system.css', kind: 'tokens' }],
        waitFor: { selector: '#glossary-concepts', renderFrames: 2 },
        states: [{ name: 'add-concept', actions: [{ action: 'click', selector: '#glossary-new' }], primaryJourney: 'edit', priorityOverrideReason: 'The user opened concept creation', regions: [{ name: 'Concept editor', selector: '.glossary-dialog', role: 'primary-content', journey: 'edit' }], continuation: { kind: 'in-page', anchor: '.glossary-dialog h2', focusWithin: '.glossary-dialog', maxScrollDelta: 8 }, waitFor: { selector: 'dialog [name=name]', renderFrames: 2 } }],
      }];
      targets[0].states.push({ name: 'guidance', actions: [{ action: 'click', selector: '#glossary-guidance' }], primaryJourney: 'edit', priorityOverrideReason: 'The user opened glossary guidance', regions: [{ name: 'Guidance editor', selector: '.glossary-dialog', role: 'primary-content', journey: 'edit' }], continuation: { kind: 'in-page', anchor: '.glossary-dialog h2', focusWithin: '.glossary-dialog' }, waitFor: { selector: '#glossary-settings-form', renderFrames: 2 } });
      targets.push({ ...targets[0], name: `glossary-detail-${theme}`, url: `${base}#/glossary/shared/${primaryId}`, regions: [{ name: 'Concept meaning and equivalents', selector: '.glossary-detail', role: 'primary-content', journey: 'browse' }], waitFor: { selector: '.glossary-detail', renderFrames: 2 }, states: [{ ...targets[0].states[0], name: 'edit-concept', actions: [{ action: 'click', selector: '#glossary-edit' }] }] });
      const config = { repoRoot: root, playwrightModuleDir: path.join(runtime, 'node_modules'), cookies: [`dc2_session=${cookieFor('owner@example.test').value}`], targets, viewports: [{ name: 'narrow', width: 390, height: 844, colorScheme: theme }, { name: 'wide', width: 1440, height: 900, colorScheme: theme }], requiredCoverage: targets.flatMap((target) => ['base', ...target.states.map((state) => state.name)].flatMap((state) => ['narrow', 'wide'].map((viewport) => ({ target: target.name, state, viewport })))) };
      const configPath = path.join(temporary, 'formal.json'); await fs.writeFile(configPath, JSON.stringify(config), { mode: 0o600 });
      const formalOut = path.join(out, `formal-${theme}`); await fs.mkdir(formalOut);
      const result = await execute(process.env.GLOSSARY_FORMAL_BINARY, ['formal-ui', 'verify', '--config', configPath, '--json-out', path.join(formalOut, 'report.json'), '--markdown-out', path.join(formalOut, 'report.md'), '--screenshot-dir', path.join(formalOut, 'screenshots')], { cwd: root, maxBuffer: 1024 * 1024, env: { ...process.env, NODE_PATH: path.join(runtime, 'node_modules') } });
      await fs.writeFile(path.join(formalOut, 'receipt.json'), result.stdout);
    }, page);
  }
  await fs.writeFile(path.join(out, 'report.json'), JSON.stringify({ checks }, null, 2));
  const failures = checks.filter((item) => item.status === 'failed');
  console.log(JSON.stringify({ passed: checks.length - failures.length, failed: failures.length, failures: failures.map(({ name, evidence }) => ({ name, evidence })), report: path.join(out, 'report.json') }));
  process.exitCode = failures.length ? 1 : 0;
  await context.close();
}

try { await main(); }
catch (error) { await fs.mkdir(out, { recursive: true }); await fs.writeFile(path.join(out, 'setup-error.log'), error.stack || String(error)); console.error(JSON.stringify({ setup_failed: true, evidence: path.join(out, 'setup-error.log') })); process.exitCode = 2; }
finally {
  await browser?.close(); await edge?.close();
  if (socketServer?.listening) await new Promise((resolve) => socketServer.close(resolve));
  if (bridge && bridge.exitCode == null) { bridge.stdin.end(); await new Promise((resolve) => bridge.once('exit', resolve)); }
  if (temporary) await fs.rm(temporary, { recursive: true, force: true });
}
