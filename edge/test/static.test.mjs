import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { createStaticServer } from '../lib/static.mjs';

test('unversioned Console assets revalidate and reload changed source', async () => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-static-refresh-'));
  await fs.writeFile(path.join(directory, 'app.js'), 'old source');
  await fs.writeFile(path.join(directory, 'font.woff2'), Buffer.from('wOF2'));
  const document = '<!doctype html><title>Console</title>' + '<p>Entry document</p>'.repeat(100);
  await fs.writeFile(path.join(directory, 'index.html'), document);
  const assets = createStaticServer({ dir: directory });
  await assets.prepare();
  const server = http.createServer((request, response) => assets.handle(request, response));
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  try {
    let indexTag;
    for (const [accept, encoding] of [['identity', null], ['gzip', 'gzip'], ['br', 'br'], ['br;q=0,gzip', 'gzip'], ['*;q=0', null]]) {
      const headers = { 'accept-encoding': accept };
      const index = await fetch(`${origin}/`, { headers });
      assert.equal(index.status, 200);
      assert.equal(index.headers.get('content-encoding'), encoding);
      assert.equal(index.headers.get('vary'), 'Accept-Encoding');
      assert.equal(index.headers.get('cache-control'), 'no-cache');
      assert.equal(await index.text(), document);
      indexTag = index.headers.get('etag');
      const head = await fetch(`${origin}/index.html`, { method: 'HEAD', headers });
      assert.equal(head.status, 200);
      assert.equal(head.headers.get('etag'), indexTag);
      assert.equal(await head.text(), '');
      assert.equal(head.headers.get('content-length'), encoding ? null : String(Buffer.byteLength(document)));
      const conditional = await fetch(`${origin}/index.html`, { headers: { ...headers, 'if-none-match': `"other", W/${indexTag}` } });
      assert.equal(conditional.status, 304);
      assert.equal(await conditional.text(), '');
    }
    const replacement = document + '<p>New source</p>';
    await fs.writeFile(path.join(directory, 'index.html'), replacement);
    const replaced = await fetch(`${origin}/`, { headers: { 'if-none-match': indexTag, 'accept-encoding': 'br' } });
    assert.equal(replaced.status, 200);
    assert.equal(await replaced.text(), replacement);
    assert.notEqual(replaced.headers.get('etag'), indexTag);
    const first = await fetch(`${origin}/app.js?v=shared-theme-20260905`);
    assert.equal(first.headers.get('cache-control'), 'no-cache');
    assert.equal(await first.text(), 'old source');
    const etag = first.headers.get('etag');
    const unchanged = await fetch(`${origin}/app.js`, { headers: { 'if-none-match': etag } });
    assert.equal(unchanged.status, 304);
    await fs.writeFile(path.join(directory, 'app.js'), 'updated Console source');
    const changed = await fetch(`${origin}/app.js`, { headers: { 'if-none-match': etag } });
    assert.equal(changed.status, 200);
    assert.equal(await changed.text(), 'updated Console source');
    assert.notEqual(changed.headers.get('etag'), etag);
    const font = await fetch(`${origin}/font.woff2`);
    assert.equal(font.status, 200);
    assert.equal(font.headers.get('content-type'), 'font/woff2');
    assert.equal(font.headers.get('x-content-type-options'), 'nosniff');
    const indexFile = path.join(directory, 'index.html');
    const fixedTime = new Date('2026-01-01T00:00:00Z');
    for (const mutation of ['overwrite', 'atomic replacement']) {
      const original = '<!doctype html><p>' + 'A'.repeat(2048) + '</p>';
      const changed = original.replaceAll('A', 'B');
      await fs.writeFile(indexFile, original);
      await fs.utimes(indexFile, fixedTime, fixedTime);
      const fresh = createStaticServer({ dir: directory });
      await fresh.prepare();
      server.removeAllListeners('request');
      server.on('request', (request, response) => fresh.handle(request, response));
      assert.equal(await (await fetch(`${origin}/`)).text(), original);
      const before = await fs.stat(indexFile);
      if (mutation === 'overwrite') {
        await fs.writeFile(indexFile, changed);
        await fs.utimes(indexFile, fixedTime, fixedTime);
      } else {
        const next = path.join(directory, 'next-index.html');
        await fs.writeFile(next, changed);
        await fs.utimes(next, fixedTime, fixedTime);
        await fs.rename(next, indexFile);
      }
      const after = await fs.stat(indexFile);
      assert.equal(after.size, before.size, mutation);
      assert.equal(after.mtimeMs, before.mtimeMs, mutation);
      if (mutation === 'overwrite') {
        assert.equal(after.ino, before.ino);
        assert.notEqual(after.ctimeMs, before.ctimeMs);
      } else {
        assert.notEqual(after.ino, before.ino);
      }
      const latest = await fetch(`${origin}/index.html`);
      assert.equal(latest.status, 200, mutation);
      assert.equal(await latest.text(), changed, mutation);
    }
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    await fs.rm(directory, { recursive: true, force: true });
  }
});

test('Console source identity is exposed when configured', async () => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-static-identity-'));
  const priorDirectory = `${directory}-prior`;
  await fs.writeFile(path.join(directory, 'index.html'), '<!doctype html><title>Console</title>');
  const assets = createStaticServer({ dir: directory, sourceRevision: 'git:test-source' });
  await assets.prepare();
  const server = http.createServer((request, response) => assets.handle(request, response));
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  try {
    const response = await fetch(`http://127.0.0.1:${server.address().port}/`);
    assert.equal(response.status, 200);
    assert.equal(response.headers.get('x-ui-source-revision'), 'git:test-source');
    assert.equal(await response.text(), '<!doctype html><title>Console</title>');
    await fs.rename(directory, priorDirectory);
    await fs.mkdir(directory);
    const replacement = '<!doctype html><title>Replaced source directory</title>';
    await fs.writeFile(path.join(directory, 'index.html'), replacement);
    const changed = await fetch(`http://127.0.0.1:${server.address().port}/index.html`);
    assert.equal(changed.status, 200);
    assert.equal(changed.headers.get('x-ui-source-revision'), 'git:test-source');
    assert.equal(await changed.text(), replacement);
    await fs.unlink(path.join(directory, 'index.html'));
    const removed = await fetch(`http://127.0.0.1:${server.address().port}/`);
    assert.equal(removed.status, 404);
    const large = '<!doctype html>' + 'x'.repeat(64 * 1024);
    await fs.writeFile(path.join(directory, 'index.html'), large);
    const oversized = createStaticServer({ dir: directory });
    await oversized.prepare();
    // A fresh handler on the same real server exercises the streaming limit.
    server.removeAllListeners('request');
    server.on('request', (request, reply) => oversized.handle(request, reply));
    const streamed = await fetch(`http://127.0.0.1:${server.address().port}/`);
    assert.equal(streamed.status, 200);
    assert.equal(await streamed.text(), large);
    if (process.platform === 'linux') {
      await fs.unlink(path.join(directory, 'index.html'));
      execFileSync('mkfifo', [path.join(directory, 'index.html')], { timeout: 1000 });
      // A separate process makes a blocked FIFO open a finite test failure.
      execFileSync(process.execPath, ['--input-type=module', '--eval', `
        import assert from 'node:assert/strict';
        import http from 'node:http';
        import { createStaticServer } from ${JSON.stringify(new URL('../lib/static.mjs', import.meta.url).href)};
        const assets = createStaticServer({dir:${JSON.stringify(directory)}});
        await assets.prepare();
        const server = http.createServer((request, response) => assets.handle(request, response));
        await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
        try { assert.equal((await fetch('http://127.0.0.1:' + server.address().port)).status, 404); }
        finally { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
      `], { timeout: 2000, stdio: 'pipe' });
    }
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    await fs.rm(directory, { recursive: true, force: true });
    await fs.rm(priorDirectory, { recursive: true, force: true });
  }
});

test('prepared entry document respects changed filesystem read permissions', async (t) => {
  if (process.getuid?.() === 0) return t.skip('root retains read authority after chmod');
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-static-permission-'));
  const index = path.join(directory, 'index.html');
  const document = '<!doctype html><title>Permission fixture</title>';
  await fs.writeFile(index, document);
  const assets = createStaticServer({ dir: directory });
  await assets.prepare();
  const server = http.createServer((request, response) => assets.handle(request, response));
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  try {
    await fs.chmod(index, 0);
    const denied = await fetch(origin).catch(() => null);
    assert.ok(denied === null || denied.status >= 400, 'prepared bytes must not bypass changed permissions');
    await fs.chmod(index, 0o600);
    assert.equal(await (await fetch(origin)).text(), document);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    await fs.rm(directory, { recursive: true, force: true });
  }
});
