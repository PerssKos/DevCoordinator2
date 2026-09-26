// Native Console self-validation. Reads real routes; never submits the draft form.
import fs from 'node:fs/promises';
import path from 'node:path';
import https from 'node:https';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const url = new URL(process.env.CONSOLE_VERIFY_BASE_URL);
if (url.protocol !== 'https:' || url.pathname !== '/' || url.username || url.password || url.search || url.hash) throw new Error('An exact HTTPS Console origin is required');
const source = process.env.CONSOLE_VERIFY_SOURCE_SHA256;
if (!/^[a-f0-9]{64}$/.test(source || '')) throw new Error('The accepted run source digest is required');
if (!process.env.DEVCOORDINATOR_EVIDENCE_DIR) throw new Error('Run through the native self-validation executor');
const relativeOutput = process.env.CONSOLE_VERIFY_OUT;
if (!/^target\/native-console-[a-zA-Z0-9-]+$/.test(relativeOutput || '')) throw new Error('A new repository-relative native Console artifact directory is required');
const output = path.join(root, relativeOutput);
await fs.mkdir(path.dirname(output), { recursive: true, mode: 0o700 });
await fs.mkdir(output, { mode: 0o700 });
const loopback = process.env.CONSOLE_VERIFY_LOOPBACK === '1';
const agent = new https.Agent({ keepAlive: true });
const request = (pathname, body) => new Promise((resolve, reject) => {
  const req = https.request({ agent, hostname: loopback ? '127.0.0.1' : url.hostname, servername: url.hostname, port: url.port || 443, path: pathname, method: body ? 'POST' : 'GET', headers: { Host: url.host, ...(body ? { 'Content-Type': 'application/json' } : {}) } }, res => {
    const chunks = []; let bytes = 0;
    res.on('data', chunk => { bytes += chunk.length; if (bytes > 1048576) { res.destroy(new Error('Response exceeds verification bound')); return; } chunks.push(chunk); });
    res.on('end', () => resolve({ status: res.statusCode, type: res.headers['content-type'], body: Buffer.concat(chunks), checked: Date.now() }));
    res.on('error', reject);
  });
  req.setTimeout(10000, () => req.destroy(new Error('Console response timed out')));
  req.on('error', reject); req.end(body);
});
const manifest = JSON.parse(await fs.readFile(path.join(root, 'console/locales/manifest.json')));
const locales = manifest.locales.filter(entry => entry.status === 'enabled').map(entry => entry.tag);
const { chromium } = createRequire(path.join(process.env.CONSOLE_VERIFY_PLAYWRIGHT || path.join(root, 'ci/playwright'), 'package.json'))('playwright');
const browser = await chromium.launch({ headless: true, args: loopback ? [`--host-resolver-rules=MAP ${url.hostname} 127.0.0.1`] : [] });
const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
const page = await context.newPage(); const checks = []; let pageErrors = 0;
page.on('pageerror', () => pageErrors++);
const check = (name, pass) => { checks.push({ name, passed: Boolean(pass) }); if (!pass) throw new Error(name); };
try {
  await page.goto(`${url.href}#/bugs`); await page.locator('#bug-form').waitFor();
  const draft = page.locator('#bug-form input[name="summary"]'); await draft.fill('Unsaved localization verification draft');
  const route = page.url(); await page.locator('#bug-form').evaluate(node => node.dataset.translationProof = 'kept');
  for (const tag of locales) {
    await page.locator('#language-toggle').click(); await page.locator('#language-menu input').fill(tag);
    await page.locator(`.language-option[data-locale="${tag}"]`).first().click();
    await page.waitForFunction(tag => document.documentElement.lang === tag, tag);
    check(`${tag}: locale active`, await page.locator('html').getAttribute('lang') === tag);
    check(`${tag}: route and form preserved`, page.url() === route && await page.locator('#bug-form[data-translation-proof="kept"]').count() === 1);
    check(`${tag}: unsaved draft preserved`, await draft.inputValue() === 'Unsaved localization verification draft');
    check(`${tag}: preference cookie`, (await context.cookies()).some(cookie => cookie.name === 'dc2-locale' && cookie.value === tag));
  }
  const selected = locales.at(-1);
  await page.reload(); await page.locator('#bug-form').waitFor();
  check('Preference survives reload', await page.locator('html').getAttribute('lang') === selected);
  await page.locator('#language-toggle').click();
  check('Every enabled locale is available', await page.locator('.language-results section').last().locator('.language-option').count() === locales.length);
  await page.locator('#language-menu').screenshot({ path: path.join(output, 'language-menu.png') });
  await page.keyboard.press('Escape'); check('Escape restores focus', await page.locator('#language-toggle').evaluate(node => node === document.activeElement));
  check('No page errors', pageErrors === 0);
  const response = await request('/');
  const pingResponse = await request('/api/v2/ping', '{}'); const ping = JSON.parse(pingResponse.body);
  check('Native route returns HTML', response.status === 200 && response.type?.split(';')[0] === 'text/html');
  check('Running daemon source identified', pingResponse.status === 200 && ping.ok === true && /^[a-f0-9]{40}$/.test(ping.data?.source_commit || ''));
  const paths = execFileSync('git', ['ls-files', '-z', '--', 'console'], { cwd: root, timeout: 2000, maxBuffer: 65536 }).toString('utf8').split('\0').filter(file => file && !path.basename(file).startsWith('.') && /\.(html|css|js|mjs|svg|woff2|png|ico|json|txt)$/i.test(file)).sort();
  check('Console source file list present', paths.length > 0 && paths.length <= 4096 && paths.every(file => file.startsWith('console/')));
  const assets = createHash('sha256').update('devcoordinator2-console-assets-v1\0');
  for (const file of paths) {
    const observed = await request('/' + file.slice('console/'.length).split('/').map(encodeURIComponent).join('/'));
    checks.push({name: `Published asset ${file}`, passed: observed.status === 200 && observed.body.equals(await fs.readFile(path.join(root, file)))});
    assets.update(file).update('\0').update(String(observed.body.length)).update('\0').update(createHash('sha256').update(observed.body).digest('hex')).update('\0');
  }
  await fs.writeFile(path.join(output, 'journey.json'), JSON.stringify({ checks, locales, checked_at_ms: response.checked }, null, 2), { mode: 0o600 });
  if (checks.some(check => !check.passed)) throw new Error('Published Console assets do not match the checked source; see journey.json');
  await fs.writeFile(path.join(output, 'response.html'), response.body, { mode: 0o600 });
  await fs.writeFile(path.join(output, 'delivery.json'), JSON.stringify({ version: 1, kind: 'native-console', target: 'console-localization', source_sha256: source, file: 'response.html', observed_sha256: createHash('sha256').update(response.body).digest('hex'), checked_at_ms: response.checked, access: url.href, observation: 'web_route_passed', native_console: { daemon_source_commit: ping.data.source_commit, assets_sha256: assets.digest('hex'), http_status: response.status, content_type: response.type } }), { mode: 0o600 });
  await fs.writeFile(path.join(output, 'journey.json'), JSON.stringify({ checks, locales, checked_at_ms: response.checked }, null, 2), { mode: 0o600 });
  console.log(JSON.stringify({ checks: checks.length, failures: 0 }));
} finally { await browser.close(); agent.destroy(); }
