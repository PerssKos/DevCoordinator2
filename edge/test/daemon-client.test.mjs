import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { createDaemonClient } from '../lib/daemon-client.mjs';

async function fixture(respond, invoke, { respondOnFrame = false, timeoutMs = 1000 } = {}) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'dc2-daemon-client-'));
  const socketPath = path.join(directory, 'daemon.sock');
  const sockets = new Set();
  const server = net.createServer({ allowHalfOpen: true }, (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
    const chunks = [];
    let responded = false;
    socket.on('data', (chunk) => {
      chunks.push(chunk);
      if (respondOnFrame && !responded && Buffer.concat(chunks).at(-1) === 0x0a) {
        responded = true;
        respond(socket, Buffer.concat(chunks));
      }
    });
    socket.on('end', () => {
      if (!responded) respond(socket, Buffer.concat(chunks));
    });
  });
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try {
    return await invoke(createDaemonClient({ socketPath, timeoutMs }));
  } finally {
    for (const socket of sockets) socket.destroy();
    await new Promise((resolve) => server.close(resolve));
    await fs.rm(directory, { recursive: true, force: true });
  }
}

test('daemon client emits and accepts only the strict protocol-v2 envelope', async () => {
  const response = await fixture((socket, raw) => {
    assert.equal(raw.at(-1), 0x0a);
    const request = JSON.parse(raw.subarray(0, -1));
    assert.deepEqual(Object.keys(request).sort(), ['client', 'id', 'operation', 'params', 'protocol']);
    assert.equal(request.protocol, 2);
    assert.equal(request.operation, 'test.list');
    assert.deepEqual(request.params, {});
    socket.end(`${JSON.stringify({ protocol: 2, id: request.id, ok: true, data: { runs: [] } })}\n`);
  }, (client) => client.call('test.list', {}, 'owner@example.test'));
  assert.deepEqual(response.data, { runs: [] });
});

test('daemon client rejects old, mismatched, extra, and unframed responses', async () => {
  for (const response of [
    (id) => `${JSON.stringify({ protocol: 1, id, ok: true, result: {} })}\n`,
    () => `${JSON.stringify({ protocol: 2, id: 'wrong', ok: true, data: {} })}\n`,
    (id) => `${JSON.stringify({ protocol: 2, id, ok: true, data: {}, extra: true })}\n`,
    (id) => JSON.stringify({ protocol: 2, id, ok: true, data: {} }),
  ]) {
    await assert.rejects(
      fixture((socket, raw) => {
        const request = JSON.parse(raw.subarray(0, -1));
        socket.end(response(request.id));
      }, (client) => client.call('ping')),
      /protocol-v2|newline-terminated/,
    );
  }
});

test('daemon client enforces request and response byte caps', async () => {
  await assert.rejects(
    fixture((socket) => socket.end(Buffer.alloc(256 * 1024 + 1, 0x20)),
      (client) => client.call('ping')),
    /response exceeds 256 KiB/,
  );
  await assert.rejects(
    fixture((socket) => socket.destroy(),
      (client) => client.call('test.start', { value: 'x'.repeat(64 * 1024) })),
    /request exceeds 64 KiB/,
  );
});

test('event wait remains open beyond the ordinary timeout and supports cancellation', async () => {
  const response = await fixture((socket, raw) => {
    const request = JSON.parse(raw.subarray(0, -1));
    setTimeout(() => socket.end(`${JSON.stringify({
      protocol: 2, id: request.id, ok: true,
      data: { cursor: 4, events: [], heartbeat_due: [{ filter_id: 'health', deadline_at: '2026-09-05T00:00:00Z' }] },
    })}\n`), 20);
  }, (client) => client.call('event.wait', { filters: [{ filter_id: 'health' }] }), { respondOnFrame: true });
  assert.equal(response.data.heartbeat_due[0].filter_id, 'health');

  const cancellation = new AbortController();
  await assert.rejects(
    fixture(() => {}, (client) => {
      setTimeout(() => cancellation.abort(), 20);
      return client.call('event.wait', { filters: [{ filter_id: 'health' }] }, null, { signal: cancellation.signal });
    }, { respondOnFrame: true }),
    /cancelled/,
  );
});

test('deployment mutations wait for the result while ordinary reads retain their deadline', async () => {
  const delayed = (socket, raw) => {
    const request = JSON.parse(raw.subarray(0, -1));
    setTimeout(() => socket.end(`${JSON.stringify({ protocol: 2, id: request.id, ok: true, data: {} })}\n`), 40);
  };
  for (const operation of ['apply', 'rollback', 'start', 'stop', 'restart', 'remove']) {
    const result = await fixture(delayed, (client) => client.call(`deployment.${operation}`), { timeoutMs: 10 });
    assert.equal(result.ok, true);
  }
  await assert.rejects(fixture(delayed, (client) => client.call('deployment.status'), { timeoutMs: 10 }), /timeout/);
  const cancellation = new AbortController();
  await assert.rejects(fixture(delayed, (client) => {
    setTimeout(() => cancellation.abort(), 20);
    return client.call('deployment.apply', {}, null, { signal: cancellation.signal });
  }, { timeoutMs: 10 }), /cancelled/);
});
