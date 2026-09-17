import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { createEdge, loadConfig } from '../devcoordinator2-edge.mjs';
import { canonicalJson } from '../lib/routes-store.mjs';
import { createSessionManager } from '../lib/session.mjs';

test('optional private upstream configuration stays a path in public configuration', () => {
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: 'example.test' }).upstreamAuthFile, '');
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: 'example.test', EDGE_UPSTREAM_AUTH_FILE: '/private/auth.json' }).upstreamAuthFile, '/private/auth.json');
});

test('invalid private upstream configuration fails without disclosing contents', async (context) => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-auth-validation-'));
  context.after(() => fs.rm(directory, { recursive: true, force: true }));
  const file = path.join(directory, 'auth.json');
  const invalid = [
    'secret invalid JSON',
    { schema: 2, routes: { app: 'Bearer test-secret' } },
    { schema: 1, routes: [] },
    { schema: 1, routes: { 'wrong.label': 'Bearer test-secret' } },
    { schema: 1, routes: { '-app': 'Bearer test-secret' } },
    { schema: 1, routes: { ['a'.repeat(64)]: 'Bearer test-secret' } },
    { schema: 1, routes: { app: 'Bearer test-secret\r\nInjected: bad' } },
    { schema: 1, routes: { app: 'Bearer test-secret\t' } },
    { schema: 1, routes: { app: 'Bearer test-secret\x7f' } },
    { schema: 1, routes: { app: 'Bearer test-secret\u0100' } },
    { schema: 1, routes: { app: null } },
    { schema: 1, routes: { app: '   ' } },
  ];
  for (const document of invalid) {
    await fs.writeFile(file, typeof document === 'string' ? document : JSON.stringify(document), { mode: 0o600 });
    await assert.rejects(createEdge({ upstreamAuthFile: file }), {
      message: 'upstream authorization: cannot read or validate private configuration',
    });
  }
  await assert.rejects(createEdge({ upstreamAuthFile: path.join(directory, 'missing-secret') }), {
    message: 'upstream authorization: cannot read or validate private configuration',
  });
});

test('private route credentials reach only the authorized HTTP and WebSocket upstream', async (context) => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-auth-forwarding-'));
  context.after(() => fs.rm(directory, { recursive: true, force: true }));
  const seen = [];
  const logs = [];
  const upstream = http.createServer((request, response) => {
    seen.push(request.headers);
    response.end('upstream ok');
  });
  upstream.on('upgrade', (request, socket) => {
    seen.push(request.headers);
    socket.end('HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n');
  });
  await new Promise((resolve) => upstream.listen(0, '127.0.0.1', resolve));
  context.after(() => new Promise((resolve) => { upstream.closeAllConnections(); upstream.close(resolve); }));
  const upstreamPort = upstream.address().port;
  const privateFile = path.join(directory, 'auth.json');
  const routeFile = path.join(directory, 'routes.json');
  const credentials = { app: 'Bearer fixture-private-app', second: 'Basic fixture-private-second', open: 'Bearer fixture-never-public' };
  await fs.writeFile(privateFile, JSON.stringify({ schema: 1, routes: credentials }), { mode: 0o600 });
  const payload = { generation: 1, published_at: '2026-09-07T00:00:00Z', domain: 'example.test',
    routes: ['app', 'second', 'other', 'open'].map((label) => ({
      deployment_id: `deployment-${label}`, lease_id: `lease-${label}`, component: 'web', label, domain: `${label}.example.test`,
      port: upstreamPort, scheme: 'http', auth: label === 'open' ? 'public' : 'authenticated', generation: 1,
    })), access: { owners: ['owner@example.test'], grants: [] } };
  const document = { schema: 2, payload_sha256: crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex'), ...payload };
  await fs.writeFile(routeFile, JSON.stringify(document));
  const config = { baseDomain: 'example.test', consoleHost: 'console.example.test', httpOnly: true, httpPort: 0,
    sessionSecret: 'fixture-session-secret-long-enough', oidcIssuer: 'http://127.0.0.1', oidcClientId: 'fixture',
    oidcClientSecret: 'fixture', upstreamAuthFile: privateFile, routesFile: routeFile,
    stateDir: path.join(directory, 'edge'), daemonSocket: path.join(directory, 'unused.sock') };
  const log = Object.fromEntries(['info', 'warn', 'error', 'debug'].map((level) => [level, (...args) => logs.push(args)]));
  const edge = await createEdge(config, { log });
  context.after(() => edge.close());
  const [port] = await edge.listen();
  const sessions = createSessionManager({ secret: config.sessionSecret, ttlMs: 60000, cookieName: 'dc2_session', secure: false });
  const cookie = sessions.issue({ sub: 'owner', email: 'owner@example.test' }).cookie.split(';')[0];
  const deniedCookie = sessions.issue({ sub: 'ungranted', email: 'ungranted@example.test' }).cookie.split(';')[0];
  async function request(label, { upgrade = false, authenticated = true, sessionCookie = cookie } = {}) {
    return new Promise((resolve, reject) => {
      const outgoing = http.request({ host: '127.0.0.1', port, path: '/', headers: {
        host: `${label}.example.test`, authorization: 'Bearer browser-controlled',
        ...(authenticated ? { cookie: sessionCookie } : {}), ...(upgrade ? { connection: 'Upgrade', upgrade: 'websocket' } : {}),
      } }, (response) => {
        let body = '';
        response.on('data', (chunk) => { body += chunk; });
        response.on('end', () => resolve({ status: response.statusCode, headers: response.headers, body }));
      });
      outgoing.on('upgrade', (response, socket) => {
        socket.destroy();
        resolve({ status: response.statusCode, headers: response.headers });
      });
      outgoing.on('error', reject);
      outgoing.end();
    });
  }
  for (const upgrade of [false, true]) {
    for (const label of ['app', 'second', 'other', 'open']) {
      const result = await request(label, { upgrade });
      assert.equal(result.status, upgrade ? 101 : 200);
      assert.equal(seen.at(-1).authorization, label === 'open' ? 'Bearer browser-controlled' : credentials[label]);
      assert.equal(seen.at(-1).cookie, undefined);
      for (const credential of Object.values(credentials)) assert.ok(!JSON.stringify(result).includes(credential));
    }
    const before = seen.length;
    assert.equal((await request('app', { upgrade, authenticated: false })).status, upgrade ? 403 : 302);
    assert.equal(seen.length, before);
    assert.equal((await request('app', { upgrade, sessionCookie: deniedCookie })).status, 403);
    assert.equal(seen.length, before);
  }
  for (const credential of Object.values(credentials)) {
    assert.ok(!JSON.stringify(logs).includes(credential));
    assert.ok(!JSON.stringify(edge.store.current()).includes(credential));
    assert.ok(!(await fs.readFile(path.join(config.stateDir, 'routes.last-known-good.json'), 'utf8')).includes(credential));
  }
});
