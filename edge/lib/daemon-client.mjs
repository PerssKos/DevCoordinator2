// One request, one response over the daemon's Unix socket (docs/protocol.md).
// The edge asserts the signed-in identity; the daemon trusts it only because
// the edge's peer uid is the configured edge uid.

import net from 'node:net';
import crypto from 'node:crypto';
import { readFileSync } from 'node:fs';

// Generated from the same typed catalog as daemon authorization and CLI validation.
const catalog = JSON.parse(readFileSync(new URL('./operations.json', import.meta.url), 'utf8'));

const MAX_REQUEST_BYTES = 64 * 1024;
const MAX_RESPONSE_BYTES = 256 * 1024;

function responseEnvelope(value, requestId) {
  if (!value || typeof value !== 'object' || Array.isArray(value)
      || value.protocol !== 3 || value.id !== requestId || typeof value.ok !== 'boolean') {
    throw new Error('invalid protocol-v3 daemon response');
  }
  const keys = Object.keys(value).sort().join(',');
  if (value.ok) {
    if (keys !== 'data,id,ok,protocol' || !value.data || typeof value.data !== 'object' || Array.isArray(value.data)) {
      throw new Error('invalid protocol-v3 success response');
    }
  } else if (keys !== 'error,id,ok,protocol'
      || !value.error || typeof value.error !== 'object' || Array.isArray(value.error)
      || typeof value.error.code !== 'string' || typeof value.error.message !== 'string'
      || typeof value.error.detail !== 'string') {
    throw new Error('invalid protocol-v3 error response');
  }
  return value;
}

export function createDaemonClient({ socketPath, connectTimeoutMs = 5000, timeoutMs = 10000 }) {
  function call(operation, params = {}, identity = null, { signal } = {}) {
    const metadata = catalog.operations[operation];
    if (!metadata) return Promise.reject(new Error('unknown operation'));
    return new Promise((resolve, reject) => {
      const socket = net.createConnection(socketPath);
      const chunks = [];
      let bytes = 0;
      let settled = false;
      let connected = false;
      const onAbort = () => { socket.destroy(); finish(reject, new Error('daemon request cancelled')); };
      const onTimeout = () => {
        socket.destroy();
        finish(reject, new Error(connected ? 'daemon response timeout' : 'daemon connect timeout'));
      };
      const finish = (fn, value) => {
        if (!settled) {
          settled = true;
          socket.setTimeout(0);
          socket.off('timeout', onTimeout);
          signal?.removeEventListener('abort', onAbort);
          fn(value);
        }
      };
      if (signal?.aborted) return onAbort();
      signal?.addEventListener('abort', onAbort, { once: true });
      socket.once('timeout', onTimeout);
      socket.setTimeout(connectTimeoutMs);
      socket.on('error', (error) => finish(reject, error));
      socket.on('connect', () => {
        connected = true;
        const seconds = metadata.responseTimeoutSeconds;
        socket.setTimeout(seconds === null ? 0 : seconds === 10 ? timeoutMs : seconds * 1000);
        const requestId = crypto.randomBytes(6).toString('hex');
        const request = {
          protocol: catalog.protocol,
          id: requestId,
          operation,
          params,
          client: { kind: 'edge', identity: identity || undefined },
        };
        const encoded = Buffer.from(`${JSON.stringify(request)}\n`);
        if (encoded.length > MAX_REQUEST_BYTES) {
          socket.destroy(); finish(reject, new Error('daemon request exceeds 64 KiB'));
          return;
        }
        socket.requestId = requestId;
        if (metadata.passiveWait) socket.write(encoded);
        else socket.end(encoded);
      });
      socket.on('data', (chunk) => {
        bytes += chunk.length;
        if (bytes > MAX_RESPONSE_BYTES) {
          socket.destroy(); finish(reject, new Error('daemon response exceeds 256 KiB'));
        } else {
          chunks.push(chunk);
        }
      });
      socket.on('close', () => {
        const raw = Buffer.concat(chunks);
        if (!raw.length) return finish(reject, new Error('empty daemon response'));
        if (raw.at(-1) !== 0x0a || raw.subarray(0, -1).includes(0x0a)) {
          return finish(reject, new Error('daemon response is not one newline-terminated frame'));
        }
        try {
          const parsed = JSON.parse(raw.subarray(0, -1).toString('utf8'));
          finish(resolve, responseEnvelope(parsed, socket.requestId));
        } catch (error) {
          finish(reject, error);
        }
      });
    });
  }
  return { call };
}
