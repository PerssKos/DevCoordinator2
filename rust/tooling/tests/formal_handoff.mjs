import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { spawn, execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { access, mkdir, mkdtemp, readFile, writeFile, symlink, truncate } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import { createRequire } from 'node:module';
import { captureEvidenceScreenshot } from '../../../skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const scratch = await mkdtemp(join(process.env.FORMAL_WEB_UI_HANDOFF_SCRATCH ?? process.env.TMPDIR ?? tmpdir(), 'formal-handoff-'));
const html = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><style>body{margin:0;color:#111;background:white;font:16px system-ui}main{padding:20px;grid-column:2;min-width:0}#editor{position:fixed;left:20px;top:120px;background:white;padding:12px;border:1px solid #555}#editor[hidden]{display:none}input{width:220px;max-width:100%;box-sizing:border-box}#editor-label{display:grid;gap:6px}button{min-height:32px}h1{font-size:24px}#layout{display:grid;grid-template-columns:0px minmax(0,1fr)}#nav{display:none}</style></head><body><div id="layout"><nav id="nav"></nav><main id="primary"><h1 id="heading">HDL workspace</h1><button id="label">Edit signal</button><form id="editor" role="dialog" data-ui-contextual-overlay="Inline declaration editor" hidden><h2 id="editor-heading">Edit signal</h2><label id="editor-label">Declaration<input id="declaration"></label><button type="button" id="cancel">Cancel</button></form></main></div><script>document.querySelector('#label').addEventListener('dblclick',()=>{document.querySelector('#editor').hidden=false;document.querySelector('#declaration').focus()});document.querySelector('#cancel').onclick=()=>{document.querySelector('#editor').hidden=true;document.querySelector('#label').focus()};</script></body></html>`;
const revision = createHash('sha256').update(await readFile(fileURLToPath(import.meta.url))).digest('hex');
const captureHtml = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><style nonce="fixture">body{margin:0;background:white;color:#008888;font:16px sans-serif;height:1302px}#private{position:absolute;left:12px;top:472px;width:366px;height:20px;color:#de1d48}input{color:#de1d48;border:1px solid #222;background:white}#declaration{position:absolute;left:12px;top:584px;width:340px;height:40px}#public{position:absolute;left:12px;top:500px}#shadow{position:absolute;left:12px;top:700px}iframe{position:absolute;left:12px;top:790px;width:340px;height:80px}#checkpoint{position:absolute;left:12px;top:1090px;width:300px;height:44px}</style></head><body><div id="private">SYNTHETIC_PRIVATE_ID</div><div id="public">Public action must remain readable</div><input id="declaration" value="SYNTHETIC_INPUT"><div id="shadow"></div><iframe title="Isolated fixture" src="/capture-child"></iframe><p id="checkpoint" tabindex="-1">Validation needs attention</p></body></html>`;
const variants = new Map([
  ['/grid-reserved', html.replace('grid-template-columns:0px', 'grid-template-columns:200px')],
  ['/character-wrap', html.replace('h1{font-size:24px}', 'h1{font-size:24px;width:1px;overflow-wrap:anywhere}')],
  ['/single-character', html.replace('HDL workspace</h1>', 'X</h1>')],
  ['/clipped', html.replace('button{min-height:32px}', 'button{min-height:32px;width:30px;overflow:hidden;white-space:nowrap}')],
  ['/overflow', html.replace('body{margin:0', 'body{width:2000px;margin:0')],
  ['/offscreen', html.replace('main{padding:20px;grid-column:2;min-width:0}', 'main{padding:20px;margin-top:1000px}')],
]);
const uploadedRequests = [];
let uploadMutation = null;
const uploadHtml = html.replace('<button id="label">Edit signal</button>', '<button id="label">Import measurements</button><label>Fixture file<input type="file" id="upload"></label><p id="filename"></p><p id="upload-result" data-ui-continuation-anchor>Choose a measurement file</p>').replace('</body>', `<script>document.querySelector('#upload').onchange=async event=>{const file=event.target.files[0];const result=await fetch('/uploaded',{method:'POST',headers:{'x-upload-name':file.name,'x-upload-type':file.type},body:file});if(result.ok){document.querySelector('#filename').textContent=file.name;document.querySelector('#upload-result').textContent='Measurements loaded';document.querySelector('#upload-result').dataset.ready='true';event.target.focus();}}</script></body>`);
const server = createServer((request, response) => {
  const pathname = new URL(request.url, 'http://fixture').pathname;
  if (pathname === '/upload' && uploadMutation) { uploadMutation(); uploadMutation = null; }
  if (pathname === '/uploaded') {
    const chunks = []; request.on('data', chunk => chunks.push(chunk));
    request.on('end', () => { uploadedRequests.push({ name: request.headers['x-upload-name'], type: request.headers['x-upload-type'], content: Buffer.concat(chunks).toString(), sha256: createHash('sha256').update(Buffer.concat(chunks)).digest('hex') }); response.writeHead(200); response.end('saved'); });
    return;
  }
  const status = Number(pathname.match(/^\/http-(403|404|503)(?:-overflow)?$/)?.[1] ?? 200);
  const csp = pathname === '/capture-csp' || pathname === '/capture-child' ? { 'content-security-policy': "default-src 'self';style-src 'nonce-fixture'" } : {};
  response.writeHead(status, { 'content-type': 'text/html', 'x-ui-source-revision': revision, ...csp });
  response.end(pathname === '/upload-hidden' ? uploadHtml.replace('type="file" id="upload"', 'type="file" id="upload" hidden').replace('event.target.focus()', "document.querySelector('#label').focus()") : pathname === '/upload' ? uploadHtml : pathname === '/capture-child' ? '<!doctype html><style nonce="fixture">body{background:white}input{color:#de1d48;background:white;border:1px solid #222;width:280px}</style><input value="SYNTHETIC_CHILD_INPUT">' : pathname.startsWith('/capture') ? captureHtml : pathname.endsWith('-overflow') ? variants.get('/overflow') : variants.get(pathname) ?? html);
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
async function capturePrivacy() {
  const require = createRequire(import.meta.url);
  const modules = process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES ?? join(root, 'ci/playwright/node_modules');
  const { chromium, devices } = require(join(modules, 'playwright'));
  const { PNG } = require(join(modules, 'playwright-core/lib/utilsBundle.js'));
  const browser = await chromium.launch({ headless: true });
  try {
    for (const csp of [false, true]) for (const phone of [false, true]) for (const scrolled of [false, true]) {
      const name = `capture-${csp ? 'csp' : 'plain'}-${phone ? 'phone' : 'desktop'}-${scrolled ? 'scrolled' : 'top'}`;
      const context = await browser.newContext(phone ? { ...devices['iPhone 13'] } : { viewport: { width: 390, height: 664 } });
      const page = await context.newPage();
      try {
        await page.goto(`${target.url.slice(0, -1)}/capture${csp ? '-csp' : ''}`);
        await page.locator('#checkpoint').focus();
        await page.evaluate(scrolled => {
          const existing = new CSSStyleSheet();existing.replaceSync('#public{letter-spacing:.1px}');document.adoptedStyleSheets = [existing];
          const shadow = document.querySelector('#shadow').attachShadow({ mode: 'open' });
          shadow.innerHTML = '<style nonce="fixture">input{color:#de1d48;background:white;border:1px solid #222;width:300px}</style><input value="SYNTHETIC_SHADOW_INPUT">';
          window.scrollTo({ top: scrolled ? 552 : 0, behavior: 'instant' });
        }, scrolled);
        const settings = { screenshotDir: join(scratch, name), screenshotMasks: [] };
        const captureTarget = { name, screenshotMasks: [{ selector: '#private', reason: 'synthetic identity' }] };
        const state = async () => ({ main: await page.evaluate(() => ({ scroll: { x: scrollX, y: scrollY }, focus: document.activeElement?.id, color: getComputedStyle(document.querySelector('#declaration')).color, sheets: document.adoptedStyleSheets.length, shadowSheets: document.querySelector('#shadow').shadowRoot.adoptedStyleSheets.length })), children: await Promise.all(page.frames().filter(frame => frame !== page.mainFrame()).map(frame => frame.evaluate(() => ({ color: getComputedStyle(document.querySelector('input')).color, sheets: document.adoptedStyleSheets.length })))) });
        const original = await state();
        const failures = [];
        for (const kind of ['viewport', 'full-page']) {
          const shot = await captureEvidenceScreenshot(page, captureTarget, { name: phone ? 'phone' : 'desktop' }, settings, name, kind);
          const png = PNG.sync.read(await readFile(shot.path));
          let protectedPixels = 0, maskPixels = 0, publicPixels = 0;
          for (let index = 0; index < png.data.length; index += 4) {
            const [red, green, blue] = png.data.subarray(index, index + 3);
            if (red > 150 && green < 80 && blue < 130) protectedPixels++;
            if (red === 119 && green === 119 && blue === 119) maskPixels++;
            if (red < 60 && green > 90 && blue > 90) publicPixels++;
          }
          if (protectedPixels) failures.push(`${kind}: ${protectedPixels} protected text pixels`);
          if (kind === 'full-page' && (maskPixels < 7000 || publicPixels < 100)) failures.push(`${kind}: mask or ordinary content missing`);
          try { assert.deepEqual(await state(), original); } catch { failures.push(`${kind}: screenshot changed page state`); }
        }
        await page.locator('#declaration').evaluate(element => element.style.setProperty('color', '#de1d48', 'important'));
        try { await captureEvidenceScreenshot(page, captureTarget, { name: 'color-override' }, settings, name, 'full-page'); }
        catch { failures.push('transparent text fill must allow an unrelated color override'); }
        await page.locator('#declaration').evaluate(element => { element.style.removeProperty('color'); element.style.setProperty('-webkit-text-fill-color', '#de1d48', 'important'); });
        try {
          await assert.rejects(captureEvidenceScreenshot(page, captureTarget, { name: 'blocked' }, settings, name, 'full-page'), /form redaction could not be applied/);
          await assert.rejects(access(join(settings.screenshotDir, `${name}-${name}-blocked-full-page.png`)), { code: 'ENOENT' });
        } catch { failures.push('unredacted input must block retention'); }
        await page.locator('#declaration').evaluate(element => element.style.removeProperty('-webkit-text-fill-color'));
        try { assert.deepEqual(await state(), original); } catch { failures.push('failed redaction changed page state'); }
        const screenshot = page.screenshot.bind(page);
        page.screenshot = async () => { throw new Error('synthetic capture failure'); };
        try {
          await assert.rejects(captureEvidenceScreenshot(page, captureTarget, { name: 'failed' }, settings, name, 'full-page'), /synthetic capture failure/);
          await assert.rejects(access(join(settings.screenshotDir, `${name}-${name}-failed-full-page.png`)), { code: 'ENOENT' });
          assert.deepEqual(await state(), original);
        } catch { failures.push('failed capture must restore the page without an artifact'); }
        finally { page.screenshot = screenshot; }
        if (csp) {
          const protectedColor = await page.evaluate(() => { const injected = document.createElement('style');injected.textContent = '#public{color:red!important}';document.head.append(injected);const color = getComputedStyle(document.querySelector('#public')).color;injected.remove();return color; });
          if (protectedColor !== 'rgb(0, 136, 136)') failures.push('page CSP was not preserved');
        }
        results.push({ name, passed: failures.length === 0, failures });
      } catch (error) { results.push({ name, passed: false, error: error.message }); }
      finally { await context.close(); }
    }
  } finally { await browser.close(); }
}
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
async function uploadFixtures() {
  const repository = join(scratch, 'upload-repository');
  await mkdir(join(repository, 'fixtures'), { recursive: true });
  await writeFile(join(repository, 'ui.html'), uploadHtml);
  const fixtureName = 'PRIVATE_FIXTURE_NAME.csv';
  const fixtureValue = 'time,current\n0,UPLOAD_PRIVATE_SOURCE_VALUE\n';
  const fixture = `fixtures/${fixtureName}`;
  await writeFile(join(repository, fixture), fixtureValue);
  const uploaded = full('/upload'); uploaded.repoRoot = repository;
  const uploadTarget = uploaded.targets[0];
  uploadTarget.reviewInputs = [{ path: 'ui.html', kind: 'ui-code' }];
  uploadTarget.includeBase = false;
  uploadTarget.states = [{ name: 'uploaded', actions: [{ action: 'setInputFiles', selector: '#upload', value: fixture }], waitFor: { selector: '#upload-result[data-ready=true]' }, continuation: { kind: 'in-page', anchor: '#upload-result', focusWithin: '#primary' } }];
  uploaded.requiredCoverage[0].state = 'uploaded'; uploaded.fixtureDataShapes[0].state = 'uploaded'; uploaded.fixtureDataShapes[0].conditionalDom.push('#upload-result[data-ready=true]');
  let originalFingerprint;
  const verifyPrivacy = async directory => {
    for (const name of ['report.json', 'report.md', 'journey-evidence.json', 'review-queue.json', 'formal-receipt.json', 'stdout.json']) {
      const text = await readFile(join(directory, name), 'utf8');
      for (const privateValue of [fixtureName, fixtureValue.trim(), fixture]) assert(!text.includes(privateValue), `${name} leaked upload data`);
    }
  };
  await verify('upload-real-file', uploaded, async ({ receipt, report, directory }) => {
    assert.equal(receipt.formal.result, 'passed');
    assert.deepEqual(uploadedRequests.at(-1), { name: fixtureName, type: 'text/csv', content: fixtureValue, sha256: createHash('sha256').update(fixtureValue).digest('hex') });
    originalFingerprint = report.pages[0].target.intentFingerprint;
    assert(originalFingerprint, 'Upload input must bind the review fingerprint');
    await verifyPrivacy(directory);
  });
  const renamed = structuredClone(uploaded); renamed.targets[0].states[0].actions[0].value = 'fixtures/renamed.csv';
  await writeFile(join(repository, 'fixtures/renamed.csv'), fixtureValue);
  await verify('upload-renamed-fixture-fingerprint', renamed, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'passed'); assert.notEqual(report.pages[0].target.intentFingerprint, originalFingerprint); });
  const binary = Buffer.from([0x50, 0x4b, 3, 4, 0, 0xff, 0x80, 0x0a]);
  await writeFile(join(repository, 'fixtures/measurements.xlsx'), binary);
  const excel = structuredClone(uploaded); excel.targets[0].states[0].actions[0].value = 'fixtures/measurements.xlsx';
  await verify('upload-xlsx-binary-mime', excel, ({ receipt }) => { assert.equal(receipt.formal.result, 'passed'); assert.equal(uploadedRequests.at(-1).type, 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet'); assert.equal(uploadedRequests.at(-1).sha256, createHash('sha256').update(binary).digest('hex')); });
  const hidden = structuredClone(uploaded); hidden.targets[0].url += '-hidden'; hidden.fixtureDataShapes[0].route = '/upload-hidden';
  await verify('upload-hidden-native-input', hidden, ({ receipt }) => { assert.equal(receipt.formal.result, 'passed'); assert.equal(uploadedRequests.at(-1).content, fixtureValue); });
  await writeFile(join(repository, fixture), `${fixtureValue}1,3\n`);
  await verify('upload-changed-file', uploaded, async ({ receipt, report, directory }) => {
    assert.equal(receipt.formal.result, 'passed');
    assert.notEqual(report.pages[0].target.intentFingerprint, originalFingerprint);
    assert.equal(uploadedRequests.at(-1).content, `${fixtureValue}1,3\n`);
    await verifyPrivacy(directory);
  });
  await writeFile(join(repository, 'fixtures/empty.csv'), '');
  const empty = structuredClone(uploaded); empty.targets[0].states[0].actions[0].value = 'fixtures/empty.csv';
  await verify('upload-empty-file', empty, ({ receipt }) => { assert.equal(receipt.formal.result, 'passed'); assert.equal(uploadedRequests.at(-1).content, ''); });
  for (const [name, value] of [['absolute', join(repository, fixture)], ['traversal', '../outside.csv'], ['dot-segment', 'fixtures/../ui.html'], ['array', [fixture]], ['object', { name: fixtureName, buffer: fixtureValue }], ['empty-path', ''], ['backslash', 'fixtures\\example.csv'], ['private-state', '.devcoordinator/secret.csv']]) {
    const invalid = structuredClone(uploaded); invalid.targets[0].states[0].actions[0].value = value;
    const count = uploadedRequests.length;
    await verify(`upload-invalid-${name}`, invalid, null, /setInputFiles value must be one repository-relative fixture path/);
    assert.equal(uploadedRequests.length, count);
  }
  execFileSync('mkfifo', [join(repository, 'fixtures/pipe.csv')]);
  await symlink(join(repository, fixture), join(repository, 'fixtures/link.csv'));
  await symlink(join(repository, 'fixtures'), join(repository, 'linked-fixtures'));
  await writeFile(join(repository, 'fixtures/oversized.csv'), ''); await truncate(join(repository, 'fixtures/oversized.csv'), 16 * 1024 * 1024 + 1);
  for (const [name, value] of [['special-file', 'fixtures/pipe.csv'], ['symlink', 'fixtures/link.csv'], ['symlink-directory', `linked-fixtures/${fixtureName}`], ['missing', 'fixtures/absent.csv'], ['directory', 'fixtures'], ['oversized', 'fixtures/oversized.csv']]) {
    const invalid = structuredClone(uploaded); invalid.targets[0].states[0].actions[0].value = value;
    const count = uploadedRequests.length;
    await verify(`upload-rejected-${name}`, invalid, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'incomplete'); assert.equal(report.pages[0].outcome, 'journey_contract_error'); assert.match(report.pages[0].skipReason, /unchanged regular file/); });
    assert.equal(uploadedRequests.length, count);
  }
  const beforeMutation = uploadedRequests.length;
  uploadMutation = () => writeFileSync(join(repository, fixture), 'changed after planning');
  await verify('upload-changed-after-planning', uploaded, async ({ receipt, report, directory }) => { assert.equal(receipt.formal.result, 'failed'); assert.equal(uploadedRequests.length, beforeMutation); assert(report.pages[0].actionTimings.some(action => action.action === 'setInputFiles' && action.outcome === 'failed')); await verifyPrivacy(directory); });
  const authUpload = structuredClone(uploaded); authUpload.authProfiles = [{ name: 'login', url: target.url, actions: [{ action: 'setInputFiles', selector: '#upload', value: fixture }] }];
  await verify('upload-not-authentication-action', authUpload, null, /supported only in target states/);
  const absentRoot = structuredClone(uploaded); delete absentRoot.repoRoot;
  await verify('upload-requires-root', absentRoot, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'incomplete'); assert.match(report.pages[0].skipReason, /canonical repoRoot/); });
  const noContinuation = structuredClone(uploaded); delete noContinuation.targets[0].states[0].continuation;
  await verify('upload-requires-continuation', noContinuation, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'incomplete'); assert.match(report.pages[0].skipReason, /continuation/); });
  const failedAction = structuredClone(uploaded); failedAction.targets[0].states[0].actions[0].selector = '#label';
  await verify('upload-failed-action-private', failedAction, async ({ receipt, directory }) => { assert.equal(receipt.formal.result, 'failed'); await verifyPrivacy(directory); });
}
try {
  if (!process.argv.includes('--capture-only')) await uploadFixtures();
  if (!process.argv.includes('--upload-only')) await capturePrivacy();
  if (!process.argv.includes('--capture-only') && !process.argv.includes('--upload-only')) {
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
  }
} finally { await new Promise(resolve => server.close(resolve)); }
await writeFile(join(scratch, 'results.json'), JSON.stringify({ results }, null, 2));
console.log(JSON.stringify({ scratch, results }));
assert(results.every(result => result.passed), 'Formal handoff regression fixtures failed');
