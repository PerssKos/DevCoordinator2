// Route-document store: reads the daemon's atomic snapshot, validates it,
// keeps the last valid document across daemon restarts, never clears served
// routes because of a malformed or partial file. (docs/route-document.md)

import crypto from 'node:crypto';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';

const MAX_BYTES = 2 * 1024 * 1024;
const POLL_MS = 5000;

export function validateDocument(text) {
  if (typeof text !== 'string' || text.length === 0 || text.length > MAX_BYTES) {
    throw new Error('route document is empty or oversized');
  }
  const doc = JSON.parse(text);
  if (doc?.schema !== 1 && doc?.schema !== 2) throw new Error('unsupported route schema');
  const { schema: _s, payload_sha256: sha, ...payload } = doc;
  const canonical = canonicalJson(payload);
  const expected = crypto.createHash('sha256').update(canonical).digest('hex');
  if (sha !== expected) throw new Error('route document checksum mismatch');
  if ((!Number.isSafeInteger(doc.generation) || doc.generation < 1) || !Array.isArray(doc.routes)) {
    throw new Error('route document missing generation or routes');
  }
  const domains = new Set();
  const leases = new Map();
  for (const r of doc.routes) {
    if (typeof r.domain !== 'string' || !Number.isInteger(r.port) || typeof r.deployment_id !== 'string') {
      throw new Error('route entry malformed');
    }
    if (r.port < 1 || r.port > 65535 || domains.has(r.domain)) throw new Error('route domain or port is invalid');
    domains.add(r.domain);
    if (doc.schema === 2 && r.observed !== true
      && (typeof r.lease_id !== 'string' || r.lease_id.length < 2 || r.lease_id.length > 128)) {
      throw new Error('route entry lease identity is malformed');
    }
    if (r.lease_id) {
      const identity = JSON.stringify([r.deployment_id, r.component, r.port]);
      if (leases.has(r.lease_id) && leases.get(r.lease_id) !== identity) throw new Error('route lease has conflicting ownership');
      leases.set(r.lease_id, identity);
    }
  }
  if (!doc.access || !Array.isArray(doc.access.owners) || !Array.isArray(doc.access.grants)) {
    throw new Error('route document missing access section');
  }
  return doc;
}

// Python json.dumps(sort_keys=True, separators=(",", ":")) equivalence for the
// payload produced by the Rust route publisher (strings, ints, floats, bools, null, arrays, objects).
export function canonicalJson(value) {
  if (value === null || typeof value !== 'object') return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  const keys = Object.keys(value).sort();
  return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalJson(value[k])}`).join(',')}}`;
}

export async function createRoutesStore({ file, stateDir, log }) {
  const lkgFile = path.join(stateDir, 'routes.last-known-good.json');
  let current = { schema: 2, generation: 0, routes: [], access: { owners: [], grants: [] }, domain: '' };
  let source = 'none';
  let rejectedGeneration = null;
  let lastRejected = null;
  let pendingLoad = Promise.resolve();

  async function tryLoad(candidate, label) {
    let text;
    try {
      text = await fsp.readFile(candidate, 'utf8');
    } catch {
      return false;
    }
    let doc;
    try {
      doc = validateDocument(text);
    } catch (error) {
      log?.warn?.('route document rejected', { file: candidate, error: error.message });
      return false;
    }
    if (doc.generation < current.generation || (doc.generation === current.generation && doc.payload_sha256 !== current.payload_sha256)) {
      rejectedGeneration = doc.generation;
      if (lastRejected !== doc.payload_sha256) log?.warn?.('route document older than served generation ignored', { generation: doc.generation, served: current.generation });
      lastRejected = doc.payload_sha256;
      return false;
    }
    lastRejected = null;
    rejectedGeneration = null;
    if (doc.generation === current.generation && source !== 'none') return true;

    if (label === 'live' && doc.schema === 2) {
      await fsp.mkdir(stateDir, { recursive: true });
      const tmp = `${lkgFile}.tmp`;
      await fsp.writeFile(tmp, text, { mode: 0o600 });
      await fsp.rename(tmp, lkgFile);
    }
    const accepted = path.join(stateDir, 'routes.accepted.json');
    const handle = await fsp.open(accepted + '.tmp', 'w', 0o600);
    try {
      await handle.writeFile(JSON.stringify({ schema: doc.schema, generation: doc.generation, payload_sha256: doc.payload_sha256 }));
      await handle.sync();
    } finally { await handle.close(); }
    await fsp.rename(accepted + '.tmp', accepted);
    current = doc;
    source = label;
    log?.info?.('route document loaded', { generation: doc.generation, routes: doc.routes.length, source: label });
    return true;
  }

  await fsp.mkdir(stateDir, { recursive: true });
  await tryLoad(lkgFile, 'last-known-good');
  await tryLoad(file, 'live');

  function reload() { pendingLoad = pendingLoad.catch(() => false).then(() => tryLoad(file, 'live')); return pendingLoad; }
  let timer = setInterval(() => { reload().catch(() => {}); }, POLL_MS);
  timer.unref();
  let watcher = null;
  try {
    watcher = fs.watch(path.dirname(file), () => { reload().catch(() => {}); });
    watcher.unref?.();
  } catch {
    watcher = null;
  }

  return {
    current: () => current,
    source: () => source,
    rejectedGeneration: () => rejectedGeneration,
    reload,
    close: () => { clearInterval(timer); watcher?.close(); },
  };
}
