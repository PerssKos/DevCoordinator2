import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { createEdge } from '../devcoordinator2-edge.mjs';
import { canonicalJson } from '../lib/routes-store.mjs';
import { createSessionManager } from '../lib/session.mjs';

const secret = 'isolated-application-session-test-secret';
let directory; let upstream; let edge; let port; let seen;
const cookie = createSessionManager({ secret, ttlMs: 60000, cookieName: 'dc2_session', secure: false })
  .issue({ email: 'owner@example.test', sub: 'test-owner' }).cookie.split(';')[0];

before(async () => {
  directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-app-session-'));
  upstream = http.createServer((req, res) => {
    seen = req.headers.cookie;
    if (req.url.startsWith('/challenge/')) {
      const scheme = req.url.slice('/challenge/'.length);
      res.writeHead(401, { 'www-authenticate': `${scheme} realm="private upstream"`, 'content-type': 'application/json' });
      return res.end('{"private":"challenge details"}');
    }
    if (req.url === '/invalid-sign-in') {
      res.writeHead(401, { 'content-type': 'application/json', 'set-cookie': ['kaizen_session=; Path=/; Max-Age=0', 'dc2_session=overwrite; Path=/', 'dc_flow=overwrite; Path=/'] });
      return res.end('{"message":"The name or password is incorrect."}');
    }
    if (req.url === '/forbidden') { res.writeHead(403, { 'content-type': 'application/json' }); return res.end('{"message":"Permission required."}'); }
    if (req.headers.cookie?.includes('kaizen_session=valid')) {
      res.writeHead(200, { 'content-type': 'application/json' }); return res.end('{"name":"founder"}');
    }
    res.writeHead(401); res.end();
  });
  await new Promise(resolve => upstream.listen(0, '127.0.0.1', resolve));
  const payload = { generation: 1, published_at: '2026-09-20T00:00:00Z', domain: 'example.test',
    routes: [false, true].map(publicRoute => ({ deployment_id: publicRoute ? 'd0123456789abcd02' : 'd0123456789abcd01', component: 'api',
      label: publicRoute ? 'public' : 'app', domain: publicRoute ? 'public.example.test' : 'app.example.test', port: upstream.address().port,
      scheme: 'http', auth: publicRoute ? 'public' : 'authenticated', generation: 1, lease_id: publicRoute ? 'lpublic' : 'lprivate' })),
    access: { owners: ['owner@example.test'], grants: [] } };
  await fs.writeFile(path.join(directory, 'routes.json'), JSON.stringify({ schema: 2,
    payload_sha256: crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex'), ...payload }));
  edge = await createEdge({ baseDomain: 'example.test', consoleHost: 'console.example.test', httpOnly: true, httpPort: 0,
    sessionSecret: secret, oidcIssuer: 'https://issuer.example.test', oidcClientId: '', oidcClientSecret: '', routesFile: path.join(directory, 'routes.json'),
    stateDir: path.join(directory, 'edge-state'), daemonSocket: path.join(directory, 'unused.sock'), consoleDir: '' },
  { log: { info() {}, warn() {}, error() {}, debug() {} } });
  [port] = await edge.listen();
});

after(async () => {
  await edge?.close(); upstream?.closeAllConnections();
  await new Promise(resolve => upstream?.close(resolve));
  await fs.rm(directory, { recursive: true, force: true });
});

function request(route, { appCookie = '', edgeCookie = cookie, host = 'app.example.test' } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request({ hostname: '127.0.0.1', port, path: route,
      headers: { host, cookie: [edgeCookie, appCookie].filter(Boolean).join('; ') } }, res => {
      const data = []; res.on('data', part => data.push(part));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(data).toString() }));
    }); req.on('error', reject); req.end();
  });
}

test('missing or expired application sessions retain401 after edge sign-in', async () => {
  for (const appCookie of ['', 'kaizen_session=expired']) {
    const response = await request('/api/auth/me', { appCookie });
    assert.equal(response.status, 401); assert.equal(response.body, '');
    assert.equal(response.headers['www-authenticate'], undefined);
  }
});

test('invalid application sign-in keeps its error and only application cookies', async () => {
  const response = await request('/invalid-sign-in');
  assert.equal(response.status, 401);
  assert.equal(JSON.parse(response.body).message, 'The name or password is incorrect.');
  assert.deepEqual(response.headers['set-cookie'], ['kaizen_session=; Path=/; Max-Age=0']);
});

test('valid application session passes while the edge session stays private', async () => {
  const response = await request('/api/auth/me', { appCookie: 'kaizen_session=valid; dc_flow=private-flow' });
  assert.equal(response.status, 200); assert.equal(JSON.parse(response.body).name, 'founder');
  assert.equal(seen, 'kaizen_session=valid');
});

test('protected HTTP authentication challenges still cannot open a browser password prompt', async () => {
  for (const scheme of ['Basic', 'Digest', 'Bearer']) {
    const response = await request('/challenge/' + scheme);
    assert.equal(response.status, 502);
    assert.equal(response.headers['www-authenticate'], undefined);
    assert(!response.body.includes('challenge details'));
  }
});

test('application denial and public-route challenges keep their original meanings', async () => {
  const denial = await request('/forbidden'); assert.equal(denial.status, 403);
  assert.equal(JSON.parse(denial.body).message, 'Permission required.');
  const open = await request('/challenge/Basic', { host: 'public.example.test', edgeCookie: '' });
  assert.equal(open.status, 401); assert.match(open.headers['www-authenticate'], /^Basic/);
});

test('an application session alone cannot bypass the edge access boundary', async () => {
  const response = await request('/api/auth/me', { edgeCookie: '', appCookie: 'kaizen_session=valid' });
  assert.equal(response.status, 302); assert.match(response.headers.location, /^\/auth\/login/);
});
