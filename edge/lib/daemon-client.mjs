// One request, one response over the daemon's Unix socket (docs/protocol.md).
// The edge asserts the signed-in identity; the daemon trusts it only because
// the edge's peer uid is the configured edge uid.

import net from 'node:net';
import crypto from 'node:crypto';

const MAX_REQUEST_BYTES = 64 * 1024;
const MAX_RESPONSE_BYTES = 256 * 1024;

function responseEnvelope(value, requestId) {
  if (!value || typeof value !== 'object' || Array.isArray(value)
      || value.protocol !== 2 || value.id !== requestId || typeof value.ok !== 'boolean') {
    throw new Error('invalid protocol-v2 daemon response');
  }
  const keys = Object.keys(value).sort().join(',');
  if (value.ok) {
    if (keys !== 'data,id,ok,protocol' || !value.data || typeof value.data !== 'object' || Array.isArray(value.data)) {
      throw new Error('invalid protocol-v2 success response');
    }
  } else if (keys !== 'error,id,ok,protocol'
      || !value.error || typeof value.error !== 'object' || Array.isArray(value.error)
      || typeof value.error.code !== 'string' || typeof value.error.message !== 'string'
      || typeof value.error.detail !== 'string') {
    throw new Error('invalid protocol-v2 error response');
  }
  return value;
}

export function createDaemonClient({ socketPath, connectTimeoutMs = 5000, timeoutMs = 10000 }) {
  function call(operation, params = {}, identity = null, { signal } = {}) {
    return new Promise((resolve, reject) => {
      const socket = net.createConnection(socketPath);
      const chunks = [];
      let bytes = 0;
      let settled = false;
      const onAbort = () => { socket.destroy(); finish(reject, new Error('daemon request cancelled')); };
      const finish = (fn, value) => {
        if (!settled) {
          settled = true;
          signal?.removeEventListener('abort', onAbort);
          fn(value);
        }
      };
      if (signal?.aborted) return onAbort();
      signal?.addEventListener('abort', onAbort, { once: true });
      socket.setTimeout(connectTimeoutMs, () => { socket.destroy(); finish(reject, new Error('daemon connect timeout')); });
      socket.on('error', (error) => finish(reject, error));
      socket.on('connect', () => {
        const longOperation = operation === 'event.wait' || /^deployment\.(apply|rollback|start|stop|restart|remove)$/.test(operation);
        socket.setTimeout(longOperation ? 0 : timeoutMs, () => { socket.destroy(); finish(reject, new Error('daemon response timeout')); });
        const requestId = crypto.randomBytes(6).toString('hex');
        const request = {
          protocol: 2,
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
        if (operation === 'event.wait') socket.write(encoded);
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
