import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs/promises';
import http from 'node:http';
import https from 'node:https';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { createEdge, loadConfig } from '../devcoordinator2-edge.mjs';

test('ACME configuration is optional', () => {
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: 'example.test' }).acmeWebroot, '');
  assert.deepEqual(loadConfig({ EDGE_BASE_DOMAIN: 'example.test' }).acmeAdditionalHosts, []);
  assert.deepEqual(loadConfig({ EDGE_BASE_DOMAIN: 'example.test', EDGE_ACME_ADDITIONAL_HOSTS: 'new.example.test' }).acmeAdditionalHosts, ['new.example.test']);
  for (const host of ['outside.test', '*.example.test', '../new.example.test', 'new..example.test']) assert.throws(() => loadConfig({ EDGE_BASE_DOMAIN: 'example.test', EDGE_ACME_ADDITIONAL_HOSTS: host }));
  assert.equal(loadConfig({ EDGE_BASE_DOMAIN: 'example.test', EDGE_ACME_WEBROOT: '/fixture/acme' }).acmeWebroot, '/fixture/acme');
});

test('HTTP serves only safe ACME tokens for covered hosts and retains HTTPS redirects', async (context) => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-acme-'));
  context.after(() => fs.rm(directory, { recursive: true, force: true }));
  const key = path.join(directory, 'key.pem');
  const cert = path.join(directory, 'cert.pem');
  execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
    '-keyout', key, '-out', cert, '-subj', '/CN=example.test', '-addext',
    'subjectAltName=DNS:example.test,DNS:console.example.test,DNS:probe.example.test,DNS:sentinel.example.test'], { stdio: 'ignore' });
  const webroot = path.join(directory, 'webroot');
  const challenges = path.join(webroot, '.well-known', 'acme-challenge');
  const outside = path.join(directory, 'private');
  await fs.mkdir(challenges, { recursive: true });
  await fs.mkdir(outside);
  await fs.writeFile(path.join(challenges, 'token_123-Abc'), 'token_123-Abc.account-thumbprint');
  await fs.writeFile(path.join(outside, 'private-token'), 'never-expose-this-private-content');
  await fs.symlink(path.join(outside, 'private-token'), path.join(challenges, 'linked-token'));
  await fs.mkdir(path.join(challenges, 'directory-token'));
  const logs = [];
  const log = Object.fromEntries(['info', 'warn', 'error', 'debug'].map((level) => [level, (...args) => logs.push(args)]));
  const config = { baseDomain: 'example.test', consoleHost: 'console.example.test', httpOnly: false,
    httpPort: 0, httpsPort: 0, tlsCert: cert, tlsKey: key, acmeWebroot: webroot,
    sessionSecret: 'fixture-session-secret-long-enough', oidcIssuer: 'http://127.0.0.1',
    oidcClientId: 'fixture', oidcClientSecret: 'fixture', routesFile: path.join(directory, 'missing-routes.json'),
    stateDir: path.join(directory, 'state'), daemonSocket: path.join(directory, 'unused.sock') };
  const edge = await createEdge(config, { log });
  context.after(() => edge.close());
  const [httpsPort, httpPort] = await edge.listen();
  async function request(pathname, { host = 'example.test', method = 'GET', secure = false, port = httpPort } = {}) {
    return new Promise((resolve, reject) => {
      const outgoing = (secure ? https : http).request({ host: '127.0.0.1', port: secure ? httpsPort : port,
        path: pathname, method, rejectUnauthorized: false, headers: { host } }, (response) => {
        let body = '';
        response.on('data', (chunk) => { body += chunk; });
        response.on('end', () => resolve({ status: response.statusCode, headers: response.headers, body }));
      });
      outgoing.on('error', reject);
      outgoing.end();
    });
  }
  const challenge = '/.well-known/acme-challenge/token_123-Abc';
  for (const host of ['example.test', 'console.example.test', 'probe.example.test', 'sentinel.example.test']) {
    const response = await request(challenge, { host });
    assert.equal(response.status, 200);
    assert.equal(response.body, 'token_123-Abc.account-thumbprint');
    assert.equal(response.headers.location, undefined);
    assert.equal(response.headers['content-type'], 'text/plain; charset=utf-8');
  }
  const head = await request(challenge, { method: 'HEAD' });
  assert.equal((await request(challenge, { host: 'new.example.test' })).status, 404);
  const bootstrap = await createEdge({ ...config, acmeAdditionalHosts: ['new.example.test'], stateDir: path.join(directory, 'bootstrap-state') }, { log });
  context.after(() => bootstrap.close());
  const [, bootstrapPort] = await bootstrap.listen();
  const initial = await request(challenge, { host: 'new.example.test', port: bootstrapPort });
  assert.equal(initial.status, 200);
  assert.equal(initial.body, 'token_123-Abc.account-thumbprint');
  assert.equal((await request(challenge, { host: 'outside.test', port: bootstrapPort })).status, 404);
  assert.equal((await request('/ordinary', { host: 'new.example.test', port: bootstrapPort })).status, 302);
  assert.equal(head.status, 200);
  assert.equal(head.body, '');
  assert.equal(Number(head.headers['content-length']), Buffer.byteLength('token_123-Abc.account-thumbprint'));
  assert.equal((await request(`${challenge}?check=1`)).status, 200);
  for (const host of ['unknown.example.test', 'outside.test', 'example.test.outside.test']) {
    assert.equal((await request(challenge, { host })).status, 404);
  }
  for (const token of ['', '..', '../private-token', '%2e%2e%2fprivate-token', 'token%00', 'linked-token', 'directory-token', 'missing-token']) {
    const response = await request(`/.well-known/acme-challenge/${token}`);
    assert.equal(response.status, 404);
    assert.equal(response.body, '');
  }
  assert.equal((await request(challenge, { method: 'POST' })).status, 404);
  assert.equal((await request(challenge, { secure: true })).status, 404);
  const ordinary = await request('/status?check=1');
  assert.equal(ordinary.status, 302);
  assert.equal(ordinary.headers.location, 'https://example.test/status?check=1');
  const canary = await createEdge({ ...config, httpOnly: true, stateDir: path.join(directory, 'canary-state') }, { log });
  context.after(() => canary.close());
  const [canaryPort] = await canary.listen();
  assert.equal((await request(challenge, { port: canaryPort })).status, 200);
  assert.equal((await request(challenge, { port: canaryPort, host: 'unknown.example.test' })).status, 404);
  assert.equal((await request('/ordinary', { port: canaryPort })).status, 404);
  await fs.rename(challenges, `${challenges}-original`);
  await fs.symlink(outside, challenges);
  const escaped = await request('/.well-known/acme-challenge/private-token');
  assert.equal(escaped.status, 404);
  assert.equal(escaped.body, '');
  assert.ok(!JSON.stringify(logs).includes('never-expose-this-private-content'));
  const disabled = await createEdge({ ...config, acmeWebroot: '', stateDir: path.join(directory, 'disabled-state') }, { log });
  context.after(() => disabled.close());
  const [, disabledPort] = await disabled.listen();
  assert.equal((await request(challenge, { port: disabledPort })).status, 302);
});
