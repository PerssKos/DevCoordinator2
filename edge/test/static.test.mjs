import assert from 'node:assert/strict';
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
  const assets = createStaticServer({ dir: directory });
  const server = http.createServer((request, response) => assets.handle(request, response));
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  try {
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
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    await fs.rm(directory, { recursive: true, force: true });
  }
});

test('Console source identity is exposed when configured', async () => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-static-identity-'));
  await fs.writeFile(path.join(directory, 'index.html'), '<!doctype html><title>Console</title>');
  const assets = createStaticServer({ dir: directory, sourceRevision: 'git:test-source' });
  const server = http.createServer((request, response) => assets.handle(request, response));
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  try {
    const response = await fetch(`http://127.0.0.1:${server.address().port}/`);
    assert.equal(response.status, 200);
    assert.equal(response.headers.get('x-ui-source-revision'), 'git:test-source');
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    await fs.rm(directory, { recursive: true, force: true });
  }
});
