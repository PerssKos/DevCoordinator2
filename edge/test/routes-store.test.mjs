import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { createRoutesStore, canonicalJson, validateDocument } from '../lib/routes-store.mjs';

function document(generation, port, schema = 2) {
  const route = { domain: 'finance.example.test', label: 'finance', deployment_id: 'dfinance', component: 'app', port, auth: 'public', scheme: 'http', generation: 3 };
  if (schema === 2) route.lease_id = 'lfinance';
  const payload = { generation, published_at: '2026-09-14T00:00:00Z', domain: 'example.test', routes: [route], access: {owners: [], grants: []} };
  return JSON.stringify({ schema, payload_sha256: crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex'), ...payload });
}

test('recovery replaces an older cached port only with a newer lease-bound document', async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-route-recovery-'));
  const stateDir = path.join(root, 'edge');
  await fs.mkdir(stateDir);
  const file = path.join(root, 'routes.json');
  const cached = document(1071, 40014, 1);
  await fs.writeFile(path.join(stateDir, 'routes.last-known-good.json'), cached);
  await fs.writeFile(file, document(259, 40000));
  const store = await createRoutesStore({file, stateDir});
  try {
    assert.equal(store.current().routes[0].port, 40014);
    assert.equal(store.rejectedGeneration(), 259);
    const repaired = document(1789344000000, 40000);
    await fs.writeFile(file, repaired);
    assert.equal(await store.reload(), true);
    assert.equal(store.current().routes[0].port, 40000);
    assert.equal(store.current().routes[0].lease_id, 'lfinance');
    assert.equal(store.rejectedGeneration(), null);
    assert.equal(await fs.readFile(path.join(stateDir, 'routes.last-known-good.json'), 'utf8'), repaired);
    const ack = JSON.parse(await fs.readFile(path.join(stateDir, 'routes.accepted.json'), 'utf8'));
    assert.equal(ack.generation, store.current().generation);
    assert.equal(ack.payload_sha256, store.current().payload_sha256);
    // Same revision with different contents cannot silently become successful.
    await fs.writeFile(file, document(1789344000000, 40014));
    assert.equal(await store.reload(), false);
    assert.equal(store.current().routes[0].port, 40000);
  } finally { store.close(); await fs.rm(root,{recursive:true,force:true}); }
});

test('schema 2 requires managed lease identities and rejects ambiguous domains', () => {
  const valid = JSON.parse(document(2, 40000));
  assert.equal(validateDocument(JSON.stringify(valid)).schema, 2);
  function sign(value) { const {schema, payload_sha256, ...payload} = value; return JSON.stringify({...value, payload_sha256: crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex')}); }
  delete valid.routes[0].lease_id;
  assert.throws(() => validateDocument(sign(valid)), /lease identity/);
  valid.routes[0].lease_id = 'lfinance'; valid.routes.push({...valid.routes[0]});
  assert.throws(() => validateDocument(sign(valid)), /domain or port/);
});
