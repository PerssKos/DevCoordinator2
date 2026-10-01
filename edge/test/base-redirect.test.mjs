import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { createEdge, loadConfig } from '../devcoordinator2-edge.mjs';
import { canonicalJson } from '../lib/routes-store.mjs';
import { createSessionManager } from '../lib/session.mjs';

test('base redirect preserves dedicated public tickets and private authentication boundaries', async (context) => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-base-redirect-'));
  context.after(() => fs.rm(directory, { recursive: true, force: true }));
  const daemonSocket = path.join(directory, 'daemon.sock');
  const daemonCalls = [];
  const daemon = net.createServer(socket => {
    let frame = '';
    socket.on('data', chunk => {
      frame += chunk;
      if (!frame.endsWith('\n')) return;
      const call = JSON.parse(frame);
      daemonCalls.push(call);
      socket.end(`${JSON.stringify({ protocol: 2, id: call.id, ok: true, data: { tickets: [] } })}\n`);
    });
  });
  await new Promise(resolve => daemon.listen(daemonSocket, resolve));
  context.after(() => new Promise(resolve => daemon.close(resolve)));
  const routesFile = path.join(directory, 'routes.json');
  const payload = { generation: 1, published_at: '2026-10-01T00:00:00Z', domain: 'example.test',
    routes: [{ deployment_id: 'deployment-private', lease_id: 'lease-private', component: 'web',
      label: 'app', domain: 'app.example.test', port: 9, scheme: 'http', auth: 'authenticated', generation: 1 }],
    access: { owners: ['owner@example.test'], grants: [] } };
  await fs.writeFile(routesFile, JSON.stringify({ schema: 2,
    payload_sha256: crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex'), ...payload }));
  const environment = { EDGE_BASE_DOMAIN: 'example.test', EDGE_HTTP_ONLY: '1', EDGE_HTTP_PORT: '0',
    EDGE_LISTEN_HOST: '127.0.0.1', EDGE_SESSION_SECRET: 'fixture-session-secret-long-enough',
    EDGE_ROUTES_FILE: routesFile, EDGE_STATE_DIR: path.join(directory, 'state'),
    EDGE_DAEMON_SOCKET: daemonSocket, EDGE_CONSOLE_DIR: fileURLToPath(new URL('../../console/', import.meta.url)) };
  assert.equal(loadConfig(environment).baseRedirect, false);
  async function start(config) {
    const edge = await createEdge(config, { log: { info() {}, warn() {}, error() {} } });
    context.after(() => edge.close());
    const [port] = await edge.listen();
    return { edge, port };
  }
  async function request(port, pathname, host = 'example.test', { method = 'GET', body, cookie, captureBody = false } = {}) {
    return new Promise((resolve, reject) => {
      const outgoing = http.request({ host: '127.0.0.1', port, path: pathname, method,
        headers: { host, ...(body ? { 'content-type': 'application/json' } : {}), ...(cookie ? { cookie } : {}) } }, (response) => {
        const chunks = [];
        response.on('data', chunk => { if (captureBody) chunks.push(chunk); });
        response.on('end', () => resolve({ status: response.statusCode, location: response.headers.location,
          ...(captureBody ? { body: Buffer.concat(chunks).toString() } : {}) }));
      });
      outgoing.on('error', reject);
      outgoing.end(body ? JSON.stringify(body) : undefined);
    });
  }
  const disabled = await start(loadConfig(environment));
  assert.equal((await request(disabled.port, '/')).status, 404);
  const enabled = await start(loadConfig({ ...environment, EDGE_BASE_REDIRECT: '1', EDGE_STATE_DIR: path.join(directory, 'enabled') }));
  for (const pathname of ['/', '/dashboard?filter=a%20b&sort=desc', '/auth/start?rt=%2Fdashboard']) {
    assert.deepEqual(await request(enabled.port, pathname), {
      status: 301, location: `${enabled.edge.consoleOrigin}${pathname}`,
    });
  }
  for (const host of ['unknown.example.test', 'probe.example.test', 'example.test.outside.test', 'outside.test']) {
    assert.deepEqual(await request(enabled.port, '/', host), { status: 404, location: undefined });
  }
  assert.deepEqual(await request(enabled.port, '/healthz', 'console.example.test'), { status: 200, location: undefined });
  const absolute = await request(enabled.port, 'http://outside.test/path?query=1');
  assert.equal(absolute.location, `${enabled.edge.consoleOrigin}/path?query=1`);

  const ticketPath = '/.well-known/devcoordinator2/tickets';
  const ticketHtml = await fs.readFile(path.join(environment.EDGE_CONSOLE_DIR, 'tickets-public.html'), 'utf8');
  for (const host of ['example.test', 'console.example.test']) {
    for (const pathname of ['/requests', '/requests/']) {
      assert.deepEqual(await request(enabled.port, pathname, host, { captureBody: true }),
        { status: 200, location: undefined, body: ticketHtml });
    }
    for (const pathname of ['/tickets.js', '/tickets.css', '/tickets-public.js', '/design-system.css',
      '/theme.js', '/app.css', '/fonts/InterVariable.woff2', '/icons/tabler/chevron-up.svg', '/vendor/pdfjs/pdf.min.mjs']) {
      assert.deepEqual(await request(enabled.port, pathname, host), { status: 200, location: undefined }, `${host}${pathname}`);
    }
    const before = daemonCalls.length;
    assert.deepEqual(await request(enabled.port, ticketPath, host), { status: 405, location: undefined });
    assert.equal(daemonCalls.length, before, 'GET must not reach the daemon');
    const params = { action: { kind: 'list' } };
    assert.deepEqual(await request(enabled.port, ticketPath, host, { method: 'POST', body: params }),
      { status: 200, location: undefined });
    assert.equal(daemonCalls.length, before + 1);
    assert.equal(daemonCalls.at(-1).operation, 'ticket.remote');
    assert.deepEqual(daemonCalls.at(-1).params, params);
    assert.deepEqual(daemonCalls.at(-1).client, { kind: 'edge' });

    // The edge chooses the RPC and caller identity, not fields in the untrusted body.
    const injected = { ...params, operation: 'deployment.remove', target: 'local', administrator: true,
      identity: 'owner@example.test', client: { kind: 'local', identity: 'owner@example.test' } };
    await request(enabled.port, ticketPath, host, { method: 'POST', body: injected });
    assert.equal(daemonCalls.length, before + 2);
    assert.equal(daemonCalls.at(-1).operation, 'ticket.remote');
    assert.deepEqual(daemonCalls.at(-1).client, { kind: 'edge' });
    assert.deepEqual(daemonCalls.at(-1).params, injected, 'ticket validation remains the daemon contract');
  }
  const beforePrivate = daemonCalls.length;
  for (const pathname of ['/', '/requests', '/tickets.js', ticketPath]) {
    const reply = await request(enabled.port, pathname, 'app.example.test');
    assert.equal(reply.status, 302);
    assert.match(reply.location, /^\/auth\/login\?rt=/);
  }
  assert.equal((await request(enabled.port, ticketPath, 'app.example.test',
    { method: 'POST', body: { action: { kind: 'list' } } })).status, 302);
  const sessions = createSessionManager({ secret: environment.EDGE_SESSION_SECRET, ttlMs: 60000,
    cookieName: 'dc2_session', secure: false });
  const ungrantedCookie = sessions.issue({ sub: 'ungranted', email: 'ungranted@example.test' }).cookie.split(';')[0];
  assert.equal((await request(enabled.port, '/requests', 'app.example.test', { cookie: ungrantedCookie })).status, 403);
  for (const pathname of ['/', '/app.js', '/tickets-public.html']) {
    const reply = await request(enabled.port, pathname, 'console.example.test');
    assert.equal(reply.status, 302);
    assert.match(reply.location, /^\/auth\/login\?rt=/);
  }
  assert.equal((await request(enabled.port, '/auth/login', 'console.example.test')).status, 200);
  assert.equal((await request(enabled.port, '/api/v2/deployment.remove', 'console.example.test',
    { method: 'POST', body: {} })).status, 401);
  for (const pathname of ['/requests', '/tickets.js', ticketPath]) {
    assert.equal((await request(enabled.port, pathname, 'example.test.outside.test')).status, 404);
  }
  assert.equal(daemonCalls.length, beforePrivate, 'private and unrelated hosts cannot use the public ticket RPC');
  assert.ok(daemonCalls.every(call => call.operation === 'ticket.remote'));
});
