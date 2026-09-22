// Edge behavior against a fixture OIDC issuer, a fake daemon socket, and
// real upstream servers. Run: node --test edge/test
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';

import {
  createEdge,
  trustedLoopbackAgent,
  trustedLoopbackConsole,
  loadConfig,
  LOCAL_AGENT_HEADER,
  LOCAL_AGENT_HEADER_VALUE,
} from '../devcoordinator2-edge.mjs';
import { canonicalJson } from '../lib/routes-store.mjs';
import { startIssuer } from './fixture-issuer.mjs';

const BASE = 'example.test';
let tmp; let issuer; let upstream; let upstreamPort; let daemonSock; let daemon; let daemonCalls = []; let edge; let port; let pendingWaitHooks; let edgeConfig;

function document(routes, access, generation) {
  routes = routes.map(route => ({...route, lease_id: route.lease_id || 'l' + route.deployment_id + route.component}));
  const payload = { generation, published_at: '2026-01-01T00:00:00Z', domain: BASE, routes, access };
  const sha = crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex');
  return JSON.stringify({ schema: 2, payload_sha256: sha, ...payload });
}

async function publish(routes, access, generation) {
  const file = path.join(tmp, 'routes.json');
  await fs.writeFile(`${file}.tmp`, document(routes, access, generation));
  await fs.rename(`${file}.tmp`, file);
  await edge.store.reload();
}

function get(pathname, { host, cookie, method = 'GET', body } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: '127.0.0.1', port, path: pathname, method,
      headers: { host: host || `console.${BASE}`, ...(cookie ? { cookie } : {}),
        ...(body ? { 'content-type': 'application/json' } : {}) } }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString() }));
    });
    req.on('error', reject);
    if (body) req.write(JSON.stringify(body));
    req.end();
  });
}

function cookieOf(res, name) {
  const raw = [].concat(res.headers['set-cookie'] || []).find((c) => c.startsWith(`${name}=`));
  return raw ? raw.split(';')[0] : null;
}

async function signIn() {
  const start = await get(`/auth/start?rt=${encodeURIComponent('/')}`);
  assert.equal(start.status, 302);
  const flow = cookieOf(start, 'dc_flow');
  const authorize = await fetch(start.headers.location, { redirect: 'manual' });
  assert.equal(authorize.status, 302);
  const callback = new URL(authorize.headers.get('location'));
  const done = await get(`${callback.pathname}${callback.search}`, { cookie: flow });
  assert.equal(done.status, 302, done.body);
  return cookieOf(done, 'dc2_session');
}

before(async () => {
  tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-edge-'));
  issuer = await startIssuer({ claims: { email: 'dev@example.test', sub: 'sub-dev' } });
  upstream = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ host: req.headers.host, path: req.url, forwarded: req.headers['x-forwarded-host'] || null, cookie: req.headers.cookie || null, who: req.headers['x-devcoordinator2-email'] || null, route: req.headers['x-devcoordinator2-route-id'] || null, agent: req.headers[LOCAL_AGENT_HEADER] || null }));
  });
  await new Promise((r) => upstream.listen(0, '127.0.0.1', r));
  upstreamPort = upstream.address().port;
  daemonSock = path.join(tmp, 'daemon.sock');
  daemon = net.createServer((socket) => {
    let buf = '';
    socket.on('data', (c) => { buf += c; if (buf.endsWith('\n')) {
      const req = JSON.parse(buf); daemonCalls.push(req);
      assert.equal(req.protocol, 2);
      assert.deepEqual(Object.keys(req).sort(), ['client', 'id', 'operation', 'params', 'protocol']);
      if (req.operation === 'event.wait' && pendingWaitHooks) {
        socket.once('close', pendingWaitHooks.closed);
        pendingWaitHooks.received();
        return;
      }
      socket.end(`${JSON.stringify({ protocol: 2, id: req.id, ok: true, data: { echoed: req.operation, identity: req.client.identity, params: req.params } })}\n`);
    } });
  });
  await new Promise((r) => daemon.listen(daemonSock, r));
  edgeConfig = { baseDomain: BASE, consoleHost: `console.${BASE}`, httpPort: 0, httpOnly: true,
    sessionSecret: 'test-secret-at-least-16-bytes', oidcIssuer: issuer.url, oidcClientId: 'test-client',
    oidcClientSecret: 'test-secret', routesFile: path.join(tmp, 'routes.json'), stateDir: path.join(tmp, 'edge-state'),
    daemonSocket: daemonSock, consoleDir: '' };
  edge = await createEdge(edgeConfig, { log: { info() {}, warn() {}, error: (...a) => console.error('EDGE', ...a), debug() {} } });
  [port] = await edge.listen();
});

after(async () => { await edge?.close(); await issuer?.close(); upstream?.closeAllConnections?.(); await new Promise((r) => upstream?.close(r)); await new Promise((r) => daemon?.close(r)); });

test('public route proxies without sign-in; authenticated route demands sign-in', async () => {
  await publish([
    { deployment_id: 'd0123456789abcd01', component: 'api', label: 'app', domain: `app.${BASE}`, port: upstreamPort, scheme: 'http', auth: 'authenticated', generation: 1 },
    { deployment_id: 'd0123456789abcd02', component: 'api', label: 'open', domain: `open.${BASE}`, port: upstreamPort, scheme: 'http', auth: 'public', generation: 1 },
  ], { owners: ['owner@example.test'], grants: [{ identity: 'dev@example.test', deployment_id: 'd0123456789abcd01', role: 'access' }] }, 1);
  const open = await get('/x?y=1', { host: `open.${BASE}` });
  assert.equal(open.status, 200);
  assert.equal(JSON.parse(open.body).path, '/x?y=1');
  const closed = await get('/', { host: `app.${BASE}` });
  assert.equal(closed.status, 302);
  assert.match(closed.headers.location, /^\/auth\/login/);
  const missing = await get('/', { host: `nope.${BASE}` });
  assert.equal(missing.status, 404);
  assert.ok(missing.body.includes(`<code>nope.${BASE}</code>`));
  assert.ok(!missing.body.includes(`nope.${BASE}.${BASE}`));
});

test('sign-in admits the invited identity via the daemon and enforces grants per request', async () => {
  const session = await signIn();
  assert.ok(session, 'session cookie issued');
  const accept = daemonCalls.find((c) => c.operation === 'user.accept_invitation');
  assert.equal(accept.protocol, 2);
  assert.equal(accept.client.identity, 'dev@example.test');
  assert.equal(accept.client.kind, 'edge');
  const ok = await get('/hello', { host: `app.${BASE}`, cookie: session });
  assert.equal(ok.status, 200, ok.body);
  const seen = JSON.parse(ok.body);
  assert.equal(seen.forwarded, `app.${BASE}`);
  assert.match(seen.host, /^127\.0\.0\.1:\d+$/,
    'upstream Host is the loopback target so dev servers need no allowedHosts');
  assert.equal(seen.cookie, null, 'session cookie never reaches the upstream');
  assert.equal(seen.who, 'dev@example.test', 'upstream learns the verified identity');
  assert.equal(seen.route, 'd0123456789abcd01/api');
  assert.equal(JSON.parse((await get('/x', { host: `open.${BASE}` })).body).who, null, 'public routes carry no identity');
  // Revocation: republish without the grant -> denied on the next request.
  await publish(edge.store.current().routes, { owners: ['owner@example.test'], grants: [] }, 2);
  const denied = await get('/hello', { host: `app.${BASE}`, cookie: session });
  assert.equal(denied.status, 403);
  // Owners pass everywhere.
  await publish(edge.store.current().routes, { owners: ['dev@example.test'], grants: [] }, 3);
  assert.equal((await get('/hello', { host: `app.${BASE}`, cookie: session })).status, 200);
  // Console API bridge carries the identity to the daemon; unauthenticated is 401.
  const api = await get('/api/v2/deployment.list', { cookie: session, method: 'POST', body: {} });
  assert.equal(api.status, 200);
  assert.equal(JSON.parse(api.body).data.identity, 'dev@example.test');
  const capacity = await get('/api/v2/test.capacity.get', {
    cookie: session, method: 'POST', body: {},
  });
  assert.equal(capacity.status, 200);
  assert.equal(JSON.parse(capacity.body).data.echoed, 'test.capacity.get');
  assert.ok(daemonCalls.some((c) => c.operation === 'test.capacity.get'));
  const logCatalog = await get('/api/v2/test.log.catalog', {
    cookie: session, method: 'POST', body: { path: '/repo', run_id: 't-run' },
  });
  assert.equal(logCatalog.status, 200);
  assert.equal(JSON.parse(logCatalog.body).data.echoed, 'test.log.catalog');
  const failureContext = await get('/api/v2/test.log.failure_context', {
    cookie: session, method: 'POST', body: { path: '/repo', check: 'unit' },
  });
  assert.equal(failureContext.status, 200);
  assert.equal(JSON.parse(failureContext.body).data.echoed,
    'test.log.failure_context');
  assert.ok(daemonCalls.some((c) => c.operation === 'test.log.catalog'
    && c.client.identity === 'dev@example.test'));
  assert.equal((await get('/api/v2/deployment.list', { method: 'POST', body: {} })).status, 401);
  // ping is the daemon's one dotless operation and passes; other dotless words
  // are grammar garbage and never reach the daemon.
  const ping = await get('/api/v2/ping', { cookie: session, method: 'POST', body: {} });
  assert.equal(ping.status, 200);
  assert.ok(daemonCalls.some((c) => c.operation === 'ping'));
  assert.equal((await get('/api/v2/bogus', { cookie: session, method: 'POST' })).status, 400);
  assert.ok(!daemonCalls.some((c) => c.operation === 'bogus'));
  assert.equal((await get('/api/v2/test..get', { cookie: session, method: 'POST' })).status, 400);
  const legacy = await get('/api/deployment.list', { cookie: session, method: 'POST', body: {} });
  assert.equal(legacy.status, 400);
  assert.equal(JSON.parse(legacy.body).protocol, 2);
  assert.equal(JSON.parse(legacy.body).error.code, 'protocol_unsupported');
});

test('malformed, stale, or tampered route documents never clear served routes', async () => {
  const before = edge.store.current();
  const file = path.join(tmp, 'routes.json');
  await fs.writeFile(file, '{"schema": 1, "gen');
  await edge.store.reload();
  assert.equal(edge.store.current().generation, before.generation);
  const tampered = JSON.parse(document(before.routes, before.access, before.generation + 1));
  tampered.routes[0].port = 1;
  await fs.writeFile(file, JSON.stringify(tampered));
  await edge.store.reload();
  assert.equal(edge.store.current().routes[0].port, before.routes[0].port);
  await fs.writeFile(file, document(before.routes, before.access, 0));
  await edge.store.reload();
  assert.equal(edge.store.current().generation, before.generation);
  // Last-known-good copy exists for daemon-restart continuity.
  const lkg = JSON.parse(await fs.readFile(path.join(tmp, 'edge-state', 'routes.last-known-good.json'), 'utf8'));
  assert.equal(lkg.generation, before.generation);
  assert.equal((await get('/healthz')).status, 200);
});

test('disconnecting a Console event wait closes the daemon subscription', async () => {
  const session = await signIn();
  let receivedResolve; let closedResolve;
  const received = new Promise((resolve) => { receivedResolve = resolve; });
  const closed = new Promise((resolve) => { closedResolve = resolve; });
  pendingWaitHooks = { received: receivedResolve, closed: closedResolve };
  const request = http.request({
    host: '127.0.0.1', port, path: '/api/v2/event.wait', method: 'POST',
    headers: { host: `console.${BASE}`, cookie: session, 'content-type': 'application/json' },
  });
  request.on('error', () => {});
  request.end(JSON.stringify({ filters: [{ filter_id: 'health', categories: ['health'] }] }));
  await Promise.race([
    received,
    new Promise((_, reject) => setTimeout(() => reject(new Error('daemon wait was not received')), 1000)),
  ]);
  request.destroy();
  await Promise.race([
    closed,
    new Promise((_, reject) => setTimeout(() => reject(new Error('daemon wait socket stayed open')), 1000)),
  ]);
  pendingWaitHooks = null;
});

test('trusted Console access uses a direct loopback peer and never a forwarded identity', async () => {
  const origin = `http://console.${BASE}`;
  const request = (remoteAddress, headers = {}) => ({ socket: { remoteAddress }, headers });
  for (const address of ['127.0.0.1', '127.0.0.2', '::1', '::ffff:127.0.0.1']) {
    assert.equal(trustedLoopbackConsole(request(address), origin, true), true);
    assert.equal(trustedLoopbackConsole(request(address), origin, false), false);
    assert.equal(trustedLoopbackConsole(request(address, { origin }), origin, true), true);
    for (const headers of [{ 'x-forwarded-for': '127.0.0.1' }, { forwarded: 'for=127.0.0.1' },
      { origin: 'http://untrusted.example' }, { 'sec-fetch-site': 'cross-site' }]) {
      assert.equal(trustedLoopbackConsole(request(address, headers), origin, true), false);
    }
  }
  for (const peer of ['192.0.2.1', '10.0.0.2', '::ffff:192.0.2.1', '::ffff:127.not.an.ip', '']) {
    assert.equal(trustedLoopbackConsole(request(peer, { 'x-forwarded-for': '127.0.0.1' }), origin, true), false);
  }
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: BASE }).trustLocalConsole, false);
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: BASE, EDGE_TRUST_LOCAL_CONSOLE: '1' }).trustLocalConsole, true);
  await publish([{ deployment_id: 'd0123456789abcd01', component: 'api', label: 'app', domain: `app.${BASE}`, port: upstreamPort, scheme: 'http', auth: 'authenticated', generation: 1 }], { owners: ['owner@example.test'], grants: [] }, 30);
  const localEdge = await createEdge({ ...edgeConfig, trustLocalConsole: true, stateDir: path.join(tmp, 'local-edge') },
    { log: { info() {}, warn() {}, error() {}, debug() {} } });
  const [localPort] = await localEdge.listen();
  const call = (pathname, { method = 'GET', headers = {}, body } = {}) => new Promise((resolve, reject) => {
    const req = http.request({ hostname: '127.0.0.1', port: localPort, path: pathname,
      method, headers: { host: `console.${BASE}`, ...headers } }, response => {
      const chunks = [];
      response.on('data', chunk => chunks.push(chunk));
      response.on('end', () => resolve({ status: response.statusCode, text: Buffer.concat(chunks).toString() }));
    });
    req.on('error', reject);
    req.end(body);
  });
  try {
    assert.equal((await call('/')).status, 200);
    const before = daemonCalls.length;
    const local = await call('/api/v2/user.whoami', { method: 'POST', body: '{}' });
    assert.equal(local.status, 200);
    assert.equal(daemonCalls.length, before + 1);
    assert.equal(daemonCalls.at(-1).client.identity, undefined);
    assert.equal((await call('/api/v2/user.whoami', { method: 'POST', body: '{}', headers: { origin: 'http://untrusted.example' } })).status, 401);
    assert.equal((await call('/', { headers: { forwarded: 'for=127.0.0.1' } })).status, 302);
    assert.equal((await call('/', { headers: { host: `app.${BASE}` } })).status, 302);
  } finally {
    await localEdge.close();
  }
});

test('trusted local agent marker bypasses deployment auth only on direct loopback', async () => {
  const origin = `http://app.${BASE}`;
  const request = (remoteAddress, headers = {}) => ({ socket: { remoteAddress }, headers });
  const marker = { [LOCAL_AGENT_HEADER]: LOCAL_AGENT_HEADER_VALUE };
  for (const address of ['127.0.0.1', '127.0.0.2', '::1', '::ffff:127.0.0.1']) {
    assert.equal(trustedLoopbackAgent(request(address, marker), origin, true), true);
    assert.equal(trustedLoopbackAgent(request(address, marker), origin, false), false);
    assert.equal(trustedLoopbackAgent(request(address, { [LOCAL_AGENT_HEADER]: 'wrong' }), origin, true), false);
    assert.equal(trustedLoopbackAgent(request(address), origin, true), false);
    assert.equal(trustedLoopbackAgent(request(address, { ...marker, 'x-forwarded-for': '127.0.0.1' }), origin, true), false);
    assert.equal(trustedLoopbackAgent(request(address, { ...marker, origin: 'http://untrusted.example' }), origin, true), false);
    assert.equal(trustedLoopbackAgent(request(address, { ...marker, 'sec-fetch-site': 'cross-site' }), origin, true), false);
  }
  for (const peer of ['192.0.2.1', '10.0.0.2', '::ffff:192.0.2.1', '']) {
    assert.equal(trustedLoopbackAgent(request(peer, marker), origin, true), false);
  }
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: BASE }).trustLocalAgent, false);
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: BASE, EDGE_TRUST_LOCAL_AGENT: '1' }).trustLocalAgent, true);

  await publish([
    { deployment_id: 'd0123456789abcd01', component: 'api', label: 'app', domain: `app.${BASE}`, port: upstreamPort, scheme: 'http', auth: 'authenticated', generation: 40 },
  ], { owners: ['owner@example.test'], grants: [] }, 40);
  const localEdge = await createEdge({ ...edgeConfig, trustLocalAgent: true, stateDir: path.join(tmp, 'agent-edge') },
    { log: { info() {}, warn() {}, error() {}, debug() {} } });
  const [localPort] = await localEdge.listen();
  const call = (headers = {}) => new Promise((resolve, reject) => {
    const req = http.request({ hostname: '127.0.0.1', port: localPort, path: '/agent-check',
      headers: { host: `app.${BASE}`, ...headers } }, response => {
      const chunks = [];
      response.on('data', chunk => chunks.push(chunk));
      response.on('end', () => resolve({ status: response.statusCode, headers: response.headers, body: Buffer.concat(chunks).toString() }));
    });
    req.on('error', reject);
    req.end();
  });
  try {
    const accepted = await call(marker);
    assert.equal(accepted.status, 200, accepted.body);
    const seen = JSON.parse(accepted.body);
    assert.equal(seen.who, null, 'agent access does not fabricate a public identity');
    assert.equal(seen.route, 'd0123456789abcd01/api');
    assert.equal(seen.cookie, null, 'edge session cookies remain private');
    assert.equal(seen.agent, null, 'the agent marker never reaches the deployment');
    assert.equal((await call()).status, 302, 'the marker is required');
    assert.equal((await call({ ...marker, 'x-forwarded-for': '127.0.0.1' })).status, 302,
      'forwarded requests stay behind sign-in');
    assert.equal((await call({ ...marker, origin: 'http://untrusted.example' })).status, 302,
      'cross-origin browser requests stay behind sign-in');
    assert.equal((await call({ ...marker, [LOCAL_AGENT_HEADER]: 'wrong' })).status, 302,
      'the marker value is exact');
  } finally {
    await localEdge.close();
  }
});
