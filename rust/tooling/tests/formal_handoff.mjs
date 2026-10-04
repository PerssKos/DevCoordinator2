import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const scratch = await mkdtemp(join(process.env.FORMAL_WEB_UI_HANDOFF_SCRATCH ?? process.env.TMPDIR ?? tmpdir(), 'formal-handoff-'));
const html = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><style>body{margin:0;color:#111;background:white;font:16px system-ui}main{padding:20px;grid-column:2;min-width:0}#editor{position:fixed;left:20px;top:120px;background:white;padding:12px;border:1px solid #555}#editor[hidden]{display:none}input{width:220px;max-width:100%;box-sizing:border-box}#editor-label{display:grid;gap:6px}button{min-height:32px}h1{font-size:24px}#layout{display:grid;grid-template-columns:0px minmax(0,1fr)}#nav{display:none}</style></head><body><div id="layout"><nav id="nav"></nav><main id="primary"><h1 id="heading">HDL workspace</h1><button id="label">Edit signal</button><form id="editor" role="dialog" data-ui-contextual-overlay="Inline declaration editor" hidden><h2 id="editor-heading">Edit signal</h2><label id="editor-label">Declaration<input id="declaration"></label><button type="button" id="cancel">Cancel</button></form></main></div><script>document.querySelector('#label').addEventListener('dblclick',()=>{document.querySelector('#editor').hidden=false;document.querySelector('#declaration').focus()});document.querySelector('#cancel').onclick=()=>{document.querySelector('#editor').hidden=true;document.querySelector('#label').focus()};</script></body></html>`;
const revision = createHash('sha256').update(await readFile(fileURLToPath(import.meta.url))).digest('hex');
const variants = new Map([
  ['/grid-reserved', html.replace('grid-template-columns:0px', 'grid-template-columns:200px')],
  ['/character-wrap', html.replace('h1{font-size:24px}', 'h1{font-size:24px;width:1px;overflow-wrap:anywhere}')],
  ['/single-character', html.replace('HDL workspace</h1>', 'X</h1>')],
  ['/clipped', html.replace('button{min-height:32px}', 'button{min-height:32px;width:30px;overflow:hidden;white-space:nowrap}')],
  ['/overflow', html.replace('body{margin:0', 'body{width:2000px;margin:0')],
  ['/offscreen', html.replace('main{padding:20px;grid-column:2;min-width:0}', 'main{padding:20px;margin-top:1000px}')],
]);
const server = createServer((request, response) => {
  const pathname = new URL(request.url, 'http://fixture').pathname;
  const status = Number(pathname.match(/^\/http-(403|404|503)(?:-overflow)?$/)?.[1] ?? 200);
  response.writeHead(status, { 'content-type': 'text/html', 'x-ui-source-revision': revision });
  response.end(pathname.endsWith('-overflow') ? variants.get('/overflow') : variants.get(pathname) ?? html);
});
server.listen(0, '127.0.0.1');
await once(server, 'listening');
const target = {
  name: 'handoff', url: `http://127.0.0.1:${server.address().port}/`, theme: 'light',
  sourceBinding: { expected: revision }, waitFor: { selector: '#primary' },
  journeys: [{ id: 'edit-signal', frequencyPercent: 100, risk: 'normal' }], primaryJourney: 'edit-signal',
  regions: [{ selector: '#primary', role: 'primary-content', journey: 'edit-signal' }],
  reviewInputs: [{ path: 'rust/tooling/tests/formal_handoff.mjs', kind: 'ui-code' }],
};
const base = { repoRoot: root, targets: [target], viewports: [{ name: 'desktop', width: 1440, height: 900 }], performance: { ttfbMs: 10000, lcpMs: 10000, ttfbLocalOnly: false }, maxPageCount: 2 };
const geometry = (primary, heading, identifier) => [
  { id: 'primary-width', kind: 'primary-content-width', selector: primary, minWidthRatio: primary === '#primary' ? 0.75 : 0.15 },
  { id: 'heading', kind: 'readable-heading', selector: heading },
  { id: 'identifier', kind: 'readable-canonical-identifier', selector: identifier },
  { id: 'wrapping', kind: 'no-character-wrapping', selector: heading },
  { id: 'overflow', kind: 'document-horizontal-overflow', selector: primary },
  { id: 'placement', kind: 'initial-viewport-placement', selector: primary },
  { id: 'clipping', kind: 'clipping', selector: identifier },
];
const full = (pathname = '/') => structuredClone({ ...base, targets: [{ ...target, url: `${target.url.slice(0, -1)}${pathname}`, geometryAssertions: [...geometry('#primary', '#heading', '#label'), { id: 'hidden-track', kind: 'hidden-navigation-track', selector: '#nav', primarySelector: '#primary', track: { selector: '#layout', axis: 'columns', index: 0 }, maxReservedSize: 0 }] }], fixtureDataShapes: [{ id: 'workspace', revision: 'v1', target: 'handoff', route: pathname, state: 'base', conditionalDom: ['#layout', '#nav', '#primary', '#label'], layoutEffect: 'Workspace and attached hidden navigation track' }], requiredCoverage: [{ target: 'handoff', state: 'base', viewport: 'desktop', width: 1440 }] });
const results = [];
let measuredUserAgent;
async function verify(name, config, check, setupError = null) {
  const directory = join(scratch, name); await mkdir(directory, { mode: 0o700 });
  const path = join(directory, 'config.json'); await writeFile(path, JSON.stringify(config));
  const args = [join(root, 'skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs'), '--config', path, '--json-out', join(directory, 'report.json'), '--markdown-out', join(directory, 'report.md')];
  if (process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES) args.push('--playwright-module-dir', process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES);
  const child = spawn(process.execPath, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '', stderr = ''; child.stdout.on('data', chunk => { stdout += chunk; }); child.stderr.on('data', chunk => { stderr += chunk; });
  const [exitCode] = await once(child, 'exit');
  await writeFile(join(directory, 'stdout.json'), stdout); await writeFile(join(directory, 'stderr.txt'), stderr);
  try {
    const receipt = JSON.parse(stdout.trim());
    const report = JSON.parse(await readFile(join(directory, 'report.json'), 'utf8'));
    assert(Buffer.byteLength(stdout) <= 2048, 'Receipt must remain bounded');
    assert.equal(stderr, '');
    if (setupError) {
      assert.equal(exitCode, 2); assert.equal(receipt.formal.result, 'blocked');
      assert.match(report.error.message, setupError);
      results.push({ name, passed: true, exitCode }); return;
    }
    await check({ exitCode, receipt, report, directory });
    results.push({ name, passed: true, exitCode });
  } catch (error) { results.push({ name, passed: false, exitCode, error: error.message }); }
}
try {
  await Promise.all([
    verify('formal-receipt', base, ({ exitCode, receipt }) => {
      assert.equal(exitCode, 0); assert(receipt.formal, 'The verifier must emit its measured formal receipt');
      assert(['passed', 'failed', 'blocked', 'incomplete'].includes(receipt.formal.result));
    }),
    verify('double-click', { ...base, targets: [{ ...target, states: [{ name: 'editor', actions: [{ action: 'dblclick', selector: '#label' }], waitFor: { selector: '#editor:not([hidden])' }, regions: [{ selector: '#editor', role: 'primary-content', journey: 'edit-signal' }], continuation: { kind: 'in-page', anchor: '#editor-heading', focusWithin: '#editor' } }] }] }, ({ exitCode, receipt, report }) => {
      assert.equal(exitCode, 0, 'The actual double-click must open the focused editor');
      assert.equal(report.pages.length, 2); assert(report.pages.every(page => page.outcome === 'checked'));
      assert.equal(receipt.formal.result, 'incomplete', 'Undeclared shapes and geometry cannot pass');
    }),
  ]);
  const expect = expected => ({ receipt }) => assert.equal(receipt.formal.result, expected);
  for (const status of [403, 404, 503]) {
    const expectedError = full(`/http-${status}`); expectedError.targets[0].expectedHttpStatus = status;
    await verify(`expected-http-${status}`, expectedError, ({ receipt, report }) => {
      assert.equal(receipt.formal.result, 'passed');
      assert.equal(report.pages[0].status, status);
      assert.equal(report.pages[0].expectedHttpStatus, status);
      assert.equal(report.pages[0].outcome, 'checked');
      assert(report.pages[0].screenshots.viewport && report.pages[0].screenshots.fullPage);
    });
  }
  await verify('unexpected-http-error', full('/http-503'), expect('blocked'));
  const wrongStatus = full('/http-404'); wrongStatus.targets[0].expectedHttpStatus = 503;
  await verify('different-http-error', wrongStatus, expect('blocked'));
  const wrongSuccess = full(); wrongSuccess.targets[0].expectedHttpStatus = 503;
  await verify('unexpected-success-not-error-view', wrongSuccess, expect('blocked'));
  const brokenError = full('/http-503-overflow'); brokenError.targets[0].expectedHttpStatus = 503;
  await verify('http-error-layout-still-checked', brokenError, expect('failed'));
  for (const [name, value] of [['success', 200], ['string', '503'], ['range', [400, 599]], ['null', null], ['fraction', 503.5], ['outside', 600]]) {
    const invalid = full('/http-503'); invalid.targets[0].expectedHttpStatus = value;
    await verify(`http-error-invalid-${name}`, invalid, null, /expectedHttpStatus.*integer HTTP error status/);
  }
  const blanketError = full('/http-503'); blanketError.targetDefaults = { expectedHttpStatus: 503 };
  await verify('http-error-not-global-default', blanketError, null, /explicit target, not targetDefaults/);
  const stateError = full('/http-503'); stateError.targets[0].states = [{ name: 'error', expectedHttpStatus: 503 }];
  await verify('http-error-not-state-override', stateError, null, /explicit target, not a state override/);
  const unrecognizedError = full('/http-503'); unrecognizedError.targets[0].expectedHttpStatus = 503; delete unrecognizedError.targets[0].waitFor;
  await verify('http-error-needs-recovery-marker', unrecognizedError, ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'incomplete');
    assert.equal(report.pages[0].outcome, 'journey_contract_error');
    assert.equal(report.pages[0].skipReason, 'an expected HTTP error target requires an explicit waitFor.selector for its rendered recovery view');
  });
  const staleError = full('/http-503'); staleError.targets[0].expectedHttpStatus = 503; staleError.targets[0].sourceBinding.expected = 'different-error-source';
  await verify('http-error-needs-source-identity', staleError, ({ receipt, report }) => { assert.notEqual(receipt.formal.result, 'passed'); assert.equal(report.pages[0].outcome, 'stale_deployment'); });
  await verify('fresh-complete', full(), async ({ receipt, report, directory }) => {
    assert.equal(receipt.formal.result, 'passed'); assert.equal(receipt.formal.freshComplete, true);
    assert.equal(receipt.formal.coverage.requiredCells, 1); assert.equal(receipt.formal.coverage.checkedCells, 1);
    for (const key of ['sourceSha256', 'configSha256', 'verifierSha256', 'planSha256', 'candidateId']) assert.match(receipt.formal[key], /^[a-f0-9]{64}$/);
    assert.match(receipt.formal.sourceDigestScope, /not the complete repository/);
    const manifest = await readFile(join(directory, 'formal-artifacts.json'));
    assert.equal(createHash('sha256').update(manifest).digest('hex'), receipt.formal.evidence.manifestSha256);
    assert(report.pages[0].metrics.handoff.geometry.every(row => row.status === 'passed'));
    measuredUserAgent = report.pages[0].metrics.handoff.browser.userAgent;
  });
  await verify('reserved-hidden-column', full('/grid-reserved'), ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'failed'); const row = report.pages[0].metrics.handoff.geometry.find(row => row.kind === 'hidden-navigation-track');
    assert.equal(row.measurements.visible, false); assert.equal(row.measurements.reservedSize, 200); assert.equal(row.status, 'failed');
  });
  const missingShape = full(); missingShape.fixtureDataShapes[0].conditionalDom.push('#absent-shape');
  await verify('missing-shape', missingShape, expect('incomplete'));
  const missingGeometry = full(); missingGeometry.targets[0].geometryAssertions[1].selector = '#absent-heading'; missingGeometry.targets[0].geometryAssertions[1].allowance = { reason: 'Allowance cannot excuse missing evidence' };
  await verify('missing-selector-with-allowance', missingGeometry, expect('incomplete'));
  const unresolvedTrack = full(); unresolvedTrack.targets[0].geometryAssertions.at(-1).track.selector = '#primary';
  await verify('unresolved-grid-track', unresolvedTrack, expect('incomplete'));
  const partial = full(); partial.targets[0].geometryAssertions = partial.targets[0].geometryAssertions.slice(0, 1);
  await verify('partial-geometry', partial, expect('incomplete'));
  await verify('character-wrapping', full('/character-wrap'), expect('failed'));
  await verify('single-character-guard', full('/single-character'), expect('passed'));
  const allowed = full('/character-wrap'); allowed.targets[0].geometryAssertions.find(row => row.kind === 'no-character-wrapping').allowance = { reason: 'Intentional stacked fixture title' };
  await verify('intentional-stacked-text', allowed, ({ report }) => assert.equal(report.pages[0].metrics.handoff.geometry.find(row => row.kind === 'no-character-wrapping').status, 'allowed'));
  for (const [name, route, kind] of [['clipping', '/clipped', 'clipping'], ['document-overflow', '/overflow', 'document-horizontal-overflow'], ['initial-viewport-placement', '/offscreen', 'initial-viewport-placement']]) await verify(name, full(route), ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'failed'); assert.equal(report.pages[0].metrics.handoff.geometry.find(row => row.kind === kind).status, 'failed');
  });
  const width = full(); width.targets[0].geometryAssertions[0].minWidth = 2000;
  await verify('primary-width', width, expect('failed'));
  const required = full(); required.requiredCoverage[0].viewport = 'missing';
  await verify('missing-required-cell', required, expect('incomplete'));
  const development = full(); development.development = { changedPaths: ['rust/tooling/tests/formal_handoff.mjs'] };
  await verify('development-subset', development, expect('incomplete'));
  const reported = full(); reported.reportedBrowserStates = [{ id: 'reported', target: 'handoff', state: 'base', theme: 'light', viewport: base.viewports[0], device: 'desktop', userAgent: 'unknown-browser', auth: 'anonymous', zoom: 1 }];
  await verify('reported-browser-mismatch', reported, expect('incomplete'));
  reported.reportedBrowserStates[0].zoom = 2;
  await verify('browser-zoom-not-css-zoom', reported, expect('incomplete'));
  const exactBrowser = full(); exactBrowser.reportedBrowserStates = [{ id: 'exact', target: 'handoff', state: 'base', theme: 'light', viewport: base.viewports[0], device: 'desktop', userAgent: measuredUserAgent, auth: 'anonymous', zoom: 1 }];
  await verify('reported-browser-exact', exactBrowser, expect('passed'));
  const requestedEngine = structuredClone(exactBrowser); requestedEngine.reportedBrowserStates[0].engine = 'webkit';
  await verify('unsupported-explicit-renderer', requestedEngine, expect('incomplete'));
  requestedEngine.reportedBrowserStates[0].engine = 'chromium';
  await verify('measured-explicit-renderer', requestedEngine, expect('passed'));
  const noRequired = structuredClone(exactBrowser); noRequired.requiredCoverage = [];
  await verify('reported-browser-without-required-cell', noRequired, expect('incomplete'));
  const phone = full(); phone.viewports = [{ name: 'phone', device: 'iPhone 13' }]; phone.requiredCoverage = [{ target: 'handoff', state: 'base', viewport: 'phone', width: 390 }];
  await verify('actual-phone-context', phone, ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'passed'); const browser = report.pages[0].metrics.handoff.browser;
    assert.equal(browser.device, 'iPhone 13'); assert.equal(browser.emulatedDevice, true); assert.equal(browser.isMobile, true); assert(browser.touchPoints > 0); assert.equal(browser.engine, 'chromium');
  });
  const query = full('/?fixture=owner'); await verify('exact-query-route', query, expect('passed'));
  query.fixtureDataShapes[0].route = '/'; await verify('missing-query-shape', query, expect('incomplete'));
  const ambiguous = full(); ambiguous.fixtureDataShapes[0].target = undefined; ambiguous.targets.push({ ...ambiguous.targets[0], name: 'other' });
  await verify('ambiguous-shape-target', ambiguous, expect('incomplete'));
  const mismatchedSource = full(); mismatchedSource.targets[0].sourceBinding.expected = 'stale-source';
  await verify('stale-source-binding', mismatchedSource, expect('incomplete'));
  const cacheDirectory = await mkdtemp(join(tmpdir(), 'formal-handoff-cache-'));
  const cached = full(); cached.development = { cache: { directory: cacheDirectory, dataRevision: 'v1' } };
  await verify('cache-first', cached, expect('incomplete'));
  await verify('cache-hit', cached, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'incomplete'); assert.equal(report.pages[0].cache.hit, true); });
  const auth = full(); auth.targets[0].authProfile = 'signed-in'; auth.authProfiles = [{ name: 'signed-in', url: target.url, actions: [{ action: 'click', selector: '#missing-auth', timeoutMs: 10 }] }];
  await verify('auth-unavailable', auth, expect('blocked'));
  const privacy = full(); privacy.targets[0].states = [{ name: 'private-input', actions: [{ action: 'dblclick', selector: '#label' }, { action: 'fill', selector: '#declaration', value: 'SECRET_HANDOFF_INPUT' }], waitFor: { selector: '#editor:not([hidden])' }, regions: [{ selector: '#editor', role: 'primary-content', journey: 'edit-signal' }], continuation: { kind: 'in-page', anchor: '#editor-heading', focusWithin: '#editor' }, geometryAssertions: geometry('#editor', '#editor-heading', '#editor-label') }];
  privacy.fixtureDataShapes.push({ id: 'editor', revision: 'v1', target: 'handoff', route: '/', state: 'private-input', conditionalDom: ['#editor', '#declaration'], layoutEffect: 'Revealed declaration editor' });
  privacy.requiredCoverage.push({ target: 'handoff', state: 'private-input', viewport: 'desktop', width: 1440 });
  await verify('private-input-redacted', privacy, async ({ receipt, directory }) => {
    assert.equal(receipt.formal.result, 'passed');
    for (const name of ['report.json', 'report.md', 'journey-evidence.json', 'review-queue.json', 'formal-receipt.json']) assert(!(await readFile(join(directory, name), 'utf8')).includes('SECRET_HANDOFF_INPUT'), name);
  });
  const invalidDirectory = join(scratch, 'not-a-directory'); await writeFile(invalidDirectory, 'disposable fixture');
  const artifact = full(); artifact.screenshotDir = invalidDirectory;
  await verify('artifact-unavailable', artifact, expect('blocked'));
  const setup = full(); setup.targets[0].states = [{ name: 'unsupported', actions: [{ action: 'evaluate', selector: '#label' }] }];
  await verify('setup-blocked', setup, expect('blocked'));
  const failedAction = full(); failedAction.targets[0].states = [{ name: 'failed-action', actions: [{ action: 'dblclick', selector: '#absent-label', timeoutMs: 10 }], continuation: { kind: 'in-page', anchor: '#editor-heading', focusWithin: '#editor' } }];
  await verify('rendered-action-failed', failedAction, expect('failed'));
} finally { await new Promise(resolve => server.close(resolve)); }
await writeFile(join(scratch, 'results.json'), JSON.stringify({ results }, null, 2));
console.log(JSON.stringify({ scratch, results }));
assert(results.every(result => result.passed), 'Formal handoff regression fixtures failed');
