import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { createStaticServer } from '../edge/lib/static.mjs';

// Exercise real HTTP caching and an ordinary browser reload across two releases.
// All application state and files here are disposable acceptance fixtures.
export async function verifyAssetRefresh({ browser, check }) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-browser-asset-refresh-'));
  const assets = createStaticServer({ dir: root });
  let currentState = 'running';
  const server = http.createServer((request, response) => {
    if (request.url === '/state') {
      response.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' });
      response.end(JSON.stringify({ status: currentState }));
    } else assets.handle(request, response);
  });
  const context = await browser.newContext();
  const writeVersion = async (version, color) => {
    await fs.writeFile(path.join(root, 'app.js'), `fetch('/state').then(r=>r.json()).then(state=>{document.querySelector('main').textContent='${version} · '+state.status;});`);
    await fs.writeFile(path.join(root, 'app.css'), `main{color:${color}}`);
  };
  try {
    await fs.writeFile(path.join(root, 'index.html'), '<!doctype html><title>Reload acceptance</title><link rel="stylesheet" href="/app.css?v=stable"><main>Loading</main><script src="/app.js?v=stable"></script>');
    await writeVersion('Release one', '#112233');
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    const page = await context.newPage();
    const responses = [];
    page.on('response', response => {
      if (/\/app\.(js|css)/.test(response.url())) responses.push(response);
    });
    await page.goto(`http://127.0.0.1:${server.address().port}/`);
    await page.getByText('Release one · running', { exact: true }).waitFor();
    check('ordinary reload: first release renders its API state', await page.locator('main').evaluate(el => getComputedStyle(el).color) === 'rgb(17, 34, 51)');
    const first = await Promise.all(responses.map(async response => ({ url: response.url(), headers: await response.allHeaders() })));
    check('ordinary reload: unversioned scripts and styles require revalidation', first.length === 2 && first.every(response => response.headers['cache-control'] === 'no-cache'));
    currentState = 'passed';
    await writeVersion('Release two', '#334455');
    responses.length = 0;
    await page.reload();
    await page.getByText('Release two · passed', { exact: true }).waitFor();
    check('ordinary reload: new scripts render current state without a hard refresh', await page.locator('main').textContent() === 'Release two · passed');
    check('ordinary reload: new stylesheet is applied', await page.locator('main').evaluate(el => getComputedStyle(el).color) === 'rgb(51, 68, 85)');
    const second = await Promise.all(responses.map(async response => ({ url: response.url(), headers: await response.allHeaders() })));
    check('ordinary reload: both unchanged asset URLs acquire new ETags', second.length === 2 && second.every(response => first.some(prior => prior.url === response.url && prior.headers.etag !== response.headers.etag)));
  } finally {
    await context.close();
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    await fs.rm(root, { recursive: true });
  }
}
