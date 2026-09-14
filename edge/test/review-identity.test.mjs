import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { createEdge, loadConfig } from '../devcoordinator2-edge.mjs';
import { loadReviewIdentity, isReviewIdentityTarget, REVIEW_IDENTITY_HEADER } from '../lib/review-identity.mjs';
import { canonicalJson } from '../lib/routes-store.mjs';
import { createSessionManager } from '../lib/session.mjs';

async function fixture(context) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-review-identity-'));
  context.after(() => fs.rm(directory, { recursive: true, force: true }));
  const keys = crypto.generateKeyPairSync('ed25519');
  const keyFile = path.join(directory, 'fixture.pem');
  const file = path.join(directory, 'review.json');
  await fs.writeFile(keyFile, keys.privateKey.export({ type: 'pkcs8', format: 'pem' }), { mode: 0o600 });
  const policy = { schema: 1, issuer: 'https://console.example.test', identity_provider_issuer: 'http://127.0.0.1',
    kid: 'fixture-key', private_key_file: keyFile,
    routes: [{ label: 'app', deployment_id: 'deployment-app', component: 'web', audience: 'route:legacy-instance-123' }] };
  await fs.writeFile(file, JSON.stringify(policy), { mode: 0o600 });
  const route = { label: 'app', deployment_id: 'deployment-app', component: 'web', auth: 'authenticated' };
  const identity = { sub: 'immutable-subject-123', email: 'reviewer@example.test', name: 'Reviewer' };
  return { directory, keys, keyFile, file, policy, route, identity };
}

function decoded(token, publicKey) {
  const [header, payload, signature] = token.split('.');
  assert.equal(crypto.verify(null, Buffer.from(`${header}.${payload}`), publicKey, Buffer.from(signature, 'base64url')), true);
  return { header: JSON.parse(Buffer.from(header, 'base64url')), payload: JSON.parse(Buffer.from(payload, 'base64url')) };
}

test('review identity is optional and configuration retains only its private file path', () => {
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: 'example.test' }).reviewIdentityFile, '');
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: 'example.test', EDGE_REVIEW_IDENTITY_FILE: '/private/review.json' }).reviewIdentityFile, '/private/review.json');
  assert.equal(loadReviewIdentity('').assertionFor({}), null);
});

test('assertions preserve immutable identity, exact request, legacy audience and unique nonce', async (context) => {
  const f = await fixture(context);
  const signer = loadReviewIdentity(f.file, { oidcIssuer: f.policy.identity_provider_issuer });
  const request = { route: f.route, identity: f.identity, method: 'POST', target: '/api/help/reviews?lang=uk&x=%2F' };
  const first = decoded(signer.assertionFor(request), f.keys.publicKey);
  const second = decoded(signer.assertionFor(request), f.keys.publicKey);
  assert.deepEqual(first.header, { alg: 'EdDSA', kid: 'fixture-key', typ: 'spectre-review-identity+jwt', v: 1 });
  assert.deepEqual(Object.keys(first.payload).sort(), ['v', 'iss', 'idp', 'sub', 'email', 'name', 'aud', 'method', 'target', 'iat', 'exp', 'jti'].sort());
  assert.equal(first.payload.sub, f.identity.sub);
  assert.equal(first.payload.idp, f.policy.identity_provider_issuer);
  assert.equal(first.payload.aud, f.policy.routes[0].audience);
  assert.equal(first.payload.iss, f.policy.issuer);
  assert.equal(first.payload.method, request.method);
  assert.equal(first.payload.target, request.target);
  assert.equal(first.payload.exp - first.payload.iat, 30);
  assert.notEqual(first.payload.jti, second.payload.jti);
  assert.match(first.payload.jti, /^[A-Za-z0-9_-]{16,128}$/);
  await fs.writeFile(f.file, 'invalid after startup');
  assert.ok(signer.assertionFor(request), 'policy is loaded once, not reread on requests');
});

test('only exact bound authenticated routes and review targets can receive assertions', async (context) => {
  const f = await fixture(context);
  const signer = loadReviewIdentity(f.file, { oidcIssuer: f.policy.identity_provider_issuer });
  const request = { route: f.route, identity: f.identity, method: 'GET', target: '/api/help/review-context?lang=uk' };
  for (const changes of [{ auth: 'public' }, { label: 'other' }, { deployment_id: 'replacement' }, { component: 'different' }]) {
    assert.equal(signer.assertionFor({ ...request, route: { ...f.route, ...changes } }), null);
  }
  const id = 'a'.repeat(32);
  for (const target of ['/api/help/review-context?lang=uk', '/api/help/review-whoami', '/api/help/reviews',
    `/api/help/reviews/${id}`, `/api/help/reviews/${id}/messages`, `/api/help/reviews/${id}/position`,
    `/api/help/reviews/${id}/decision`, '/api/help/review-media', `/api/help/review-media/${id}/content`,
    '/api/admin/help/reviews/export']) assert.equal(isReviewIdentityTarget(target), true, target);
  for (const target of ['/api/admin/users', '/api/help/reviews/', '/api/help/reviews/../reviews',
    '/api/help/%72eviews', '//api/help/reviews', '/api/help/reviews#fragment', '/help',
    `/api/help/reviews/${id}/apply`, `/api/help/review-media/${id}`, `/api/help/reviews/${id.toUpperCase()}`]) {
    assert.equal(signer.assertionFor({ ...request, target }), null, target);
  }
  for (const identity of [null, { ...f.identity, sub: '' }, { ...f.identity, email: 'invalid' }, { ...f.identity, name: 'bad\nname' }]) {
    assert.throws(() => signer.assertionFor({ ...request, identity }), { message: 'review identity: verified request identity is unavailable' });
  }
});

test('invalid or mismatched private policy and unsafe key files fail without disclosure', async (context) => {
  const f = await fixture(context);
  const expected = { message: 'review identity: cannot read or validate private configuration' };
  assert.throws(() => loadReviewIdentity(f.file, { oidcIssuer: 'https://different-issuer.test' }), expected);
  const malformed = ['fixture-secret-invalid-json', { ...f.policy, schema: 2 }, { ...f.policy, extra: true },
    { ...f.policy, kid: null }, { ...f.policy, routes: [{ ...f.policy.routes[0], component: null }] },
    { ...f.policy, routes: [f.policy.routes[0], f.policy.routes[0]] },
    { ...f.policy, private_key_file: 'relative-secret.pem' }];
  for (const policy of malformed) {
    await fs.writeFile(f.file, typeof policy === 'string' ? policy : JSON.stringify(policy));
    assert.throws(() => loadReviewIdentity(f.file, { oidcIssuer: f.policy.identity_provider_issuer }), expected);
  }
  await fs.writeFile(f.file, JSON.stringify(f.policy));
  await fs.chmod(f.keyFile, 0o644);
  assert.throws(() => loadReviewIdentity(f.file, { oidcIssuer: f.policy.identity_provider_issuer }), expected);
  await fs.chmod(f.keyFile, 0o600);
  const link = path.join(f.directory, 'linked-key.pem');
  await fs.symlink(f.keyFile, link);
  await fs.writeFile(f.file, JSON.stringify({ ...f.policy, private_key_file: link }));
  assert.throws(() => loadReviewIdentity(f.file, { oidcIssuer: f.policy.identity_provider_issuer }), expected);
  await fs.writeFile(f.file, JSON.stringify(f.policy));
  await fs.chmod(f.file, 0o644);
  assert.throws(() => loadReviewIdentity(f.file, { oidcIssuer: f.policy.identity_provider_issuer }), expected);
});

test('real edge signs admitted reviewer requests, replaces spoofed headers and excludes all other paths and upgrades', async (context) => {
  const f = await fixture(context);
  const seen = [];
  const logs = [];
  const upstream = http.createServer((request, response) => { seen.push({ headers: request.headers, url: request.url, method: request.method }); response.end('ok'); });
  upstream.on('upgrade', (request, socket) => {
    seen.push({ headers: request.headers, url: request.url, method: request.method });
    socket.end('HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n');
  });
  await new Promise((resolve) => upstream.listen(0, '127.0.0.1', resolve));
  context.after(() => new Promise((resolve) => { upstream.closeAllConnections(); upstream.close(resolve); }));
  const routeFile = path.join(f.directory, 'routes.json');
  const payload = { generation: 1, published_at: '2026-09-14T00:00:00Z', domain: 'example.test',
    routes: ['app', 'other', 'open'].map((label) => ({ deployment_id: `deployment-${label}`, component: 'web', label,
      domain: `${label}.example.test`, port: upstream.address().port, scheme: 'http',
      auth: label === 'open' ? 'public' : 'authenticated', generation: 1 })),
    access: { owners: [], grants: ['app', 'other'].map((label) => ({ identity: f.identity.email, deployment_id: `deployment-${label}`, role: 'viewer' })) } };
  await fs.writeFile(routeFile, JSON.stringify({ schema: 1, payload_sha256: crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex'), ...payload }));
  const config = { baseDomain: 'example.test', consoleHost: 'console.example.test', httpOnly: true, httpPort: 0,
    sessionSecret: 'fixture-session-secret-long-enough', oidcIssuer: f.policy.identity_provider_issuer,
    oidcClientId: 'fixture', oidcClientSecret: 'fixture', reviewIdentityFile: f.file, routesFile: routeFile,
    stateDir: path.join(f.directory, 'edge'), daemonSocket: path.join(f.directory, 'unused.sock') };
  const log = Object.fromEntries(['info', 'warn', 'error', 'debug'].map((level) => [level, (...args) => logs.push(args)]));
  const edge = await createEdge(config, { log });
  context.after(() => edge.close());
  const [port] = await edge.listen();
  const sessions = createSessionManager({ secret: config.sessionSecret, ttlMs: 60000, cookieName: 'dc2_session', secure: false });
  const cookie = sessions.issue(f.identity).cookie.split(';')[0];
  const deniedCookie = sessions.issue({ sub: 'ungranted-subject', email: 'ungranted@example.test' }).cookie.split(';')[0];
  async function request(label = 'app', target = '/api/help/review-context?lang=uk', { method = 'GET', upgrade = false, nominate = false, sessionCookie = cookie } = {}) {
    return new Promise((resolve, reject) => {
      const outgoing = http.request({ host: '127.0.0.1', port, path: target, method, headers: {
        host: `${label}.example.test`, [REVIEW_IDENTITY_HEADER]: ['caller-forged-one', 'caller-forged-two'],
        connection: [upgrade ? 'Upgrade' : 'close', ...(nominate ? ['X-Spectre-Review-Identity'] : [])].join(', '),
        ...(sessionCookie ? { cookie: sessionCookie } : {}), ...(upgrade ? { upgrade: 'websocket' } : {}),
      } }, (response) => { response.resume(); response.on('end', () => resolve(response.statusCode)); });
      outgoing.on('upgrade', (response, socket) => { socket.destroy(); resolve(response.statusCode); });
      outgoing.on('error', reject);
      outgoing.end();
    });
  }
  for (const [method, target] of [['GET', '/api/help/review-context?lang=uk'], ['POST', '/api/help/reviews'], ['POST', `/api/help/reviews/${'a'.repeat(32)}/messages`]]) {
    assert.equal(await request('app', target, { method, nominate: true }), 200);
    const latest = seen.at(-1);
    const assertion = decoded(latest.headers[REVIEW_IDENTITY_HEADER], f.keys.publicKey);
    assert.equal(assertion.payload.sub, f.identity.sub);
    assert.equal(assertion.payload.email, f.identity.email);
    assert.equal(assertion.payload.method, method);
    assert.equal(assertion.payload.target, target);
    assert.equal(latest.headers.cookie, undefined);
    assert.equal(latest.headers[REVIEW_IDENTITY_HEADER].includes('caller-forged'), false);
  }
  for (const label of ['app', 'other', 'open']) {
    for (const target of ['/help', '/api/admin/users', '/api/help/review-context?lang=uk']) {
      if (label === 'app' && target.includes('review-context')) continue;
      assert.equal(await request(label, target), 200);
      assert.equal(seen.at(-1).headers[REVIEW_IDENTITY_HEADER], undefined);
    }
    assert.equal(await request(label, '/api/help/review-context?lang=uk', { upgrade: true }), 101);
    assert.equal(seen.at(-1).headers[REVIEW_IDENTITY_HEADER], undefined);
  }
  const before = seen.length;
  assert.equal(await request('app', undefined, { sessionCookie: null }), 302);
  assert.equal(await request('app', undefined, { sessionCookie: deniedCookie }), 403);
  assert.equal(seen.length, before);
  assert.equal(JSON.stringify(logs).includes('immutable-subject'), false);
  assert.equal(JSON.stringify(edge.store.current()).includes('legacy-instance'), false);
  assert.equal((await fs.readFile(path.join(config.stateDir, 'routes.last-known-good.json'), 'utf8')).includes('fixture-key'), false);
});
