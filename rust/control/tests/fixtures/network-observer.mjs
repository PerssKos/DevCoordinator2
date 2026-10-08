// Disposable root-acceptance observer. No application/session data is loaded.
import assert from 'node:assert/strict';
import {writeSync} from 'node:fs';
import {createServer} from 'node:http';
import {connect} from 'node:net';
import {pathToFileURL} from 'node:url';

const [gate, moduleDirectory, completion] = process.argv.slice(2);
const {chromium} = await import(pathToFileURL(moduleDirectory + '/playwright/index.mjs').href);
const pending = [], failures = [];
let armed;
const ready = new Promise(resolve => { armed = resolve; });
const server = createServer((request, response) => {
  if (request.url === '/module.mjs') {
    pending.push(response);
    if (pending.length === 3) armed();
  } else if (request.url === '/control') {
    response.end('<!doctype html><title>control</title><p>ready</p>');
  } else {
    response.setHeader('content-type', 'text/html');
    response.end('<!doctype html><title>observer</title><script type="module" src="/module.mjs"></script>');
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = 'http://127.0.0.1:' + server.address().port;
let browser, socket;
const stopped = completion === 'event' ? new Promise(resolve => process.once('SIGTERM', resolve)) : null;
try {
  browser = await chromium.launch({headless: true});
  const control = await browser.newPage();
  await control.goto(base + '/control');
  assert.equal(await control.locator('p').textContent(), 'ready');
  await control.close();
  const pages = await Promise.all(Array.from({length: 3}, async () => {
    const page = await browser.newPage();
    page.on('requestfailed', request => failures.push(/^net::ERR_[A-Z_]+$/.test(request.failure()?.errorText || '') ? request.failure().errorText : 'other'));
    await page.goto(base, {waitUntil: 'commit'});
    return page;
  }));
  await ready;
  if (completion === 'event') {
    writeSync(Number(process.env.DEVCOORDINATOR_EVENT_FD), JSON.stringify({schema: 2, run_id: process.env.DEVCOORDINATOR_RUN_ID, check: process.env.DEVCOORDINATOR_CHECK_NAME, status: 'passed', reason: null}) + '\n');
  }
  socket = connect(gate);
  await new Promise((resolve, reject) => { socket.once('connect', resolve); socket.once('error', reject); });
  socket.write('ready');
  await new Promise((resolve, reject) => { socket.once('data', resolve); socket.once('error', reject); socket.once('end', () => reject(new Error('owner closed before release'))); });
  for (const response of pending) {
    response.setHeader('content-type', 'text/javascript');
    response.end('globalThis.observerReady=true;');
  }
  await Promise.all(pages.map(page => page.waitForFunction(() => globalThis.observerReady === true, null, {timeout: 5000})));
  assert.deepEqual(failures, []);
  console.log(JSON.stringify({scope: 'isolated synthetic browser boundary', controlReady: 1, protectedPages: 3, failedRequests: failures.length}));
  if (stopped) {
    await browser.close(); browser = null;
    socket.write('done');
    // Event success is readiness. Retain the process until the executor's normal
    // service shutdown after its dependent consumer has finished.
    await stopped;
  }
} finally {
  socket?.destroy();
  await browser?.close();
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
}
