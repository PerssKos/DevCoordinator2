// Static UI file server for the DevOps Console control panel.
// Contract (docs/architecture.md): serves src/ui/; '/' -> index.html with
// Cache-Control: no-cache for unversioned assets, with conditional ETag reads;
// fixed MIME map (html/css/js/svg/png/ico/json/txt); ETag from mtime+size;
// traversal-proof (resolve + prefix check); GET/HEAD only; 404 otherwise.

import { createReadStream, constants as fsConstants } from 'node:fs';
import { open, stat } from 'node:fs/promises';
import path from 'node:path';
import { pipeline } from 'node:stream/promises';
import { promisify } from 'node:util';
import { brotliCompress, gzip, createBrotliCompress, createGzip, constants as zlibConstants } from 'node:zlib';

const MIME = new Map([
  ['.html', 'text/html; charset=utf-8'],
  ['.css', 'text/css; charset=utf-8'],
  ['.js', 'text/javascript; charset=utf-8'],
  ['.mjs', 'text/javascript; charset=utf-8'],
  ['.svg', 'image/svg+xml'],
  ['.woff2', 'font/woff2'],
  ['.ttf', 'font/ttf'],
  ['.pfb', 'application/octet-stream'],
  ['.bcmap', 'application/octet-stream'],
  ['.wasm', 'application/wasm'],
  ['.png', 'image/png'],
  ['.ico', 'image/x-icon'],
  ['.json', 'application/json; charset=utf-8'],
  ['.txt', 'text/plain; charset=utf-8'],
]);

const COMPRESSIBLE = new Set(['.html', '.css', '.js', '.mjs', '.svg', '.json', '.txt']);
const MIN_COMPRESSION_BYTES = 1024;
const MAX_PREPARED_INDEX_BYTES = 64 * 1024;
const compressBrotli = promisify(brotliCompress);
const compressGzip = promisify(gzip);

function sameFile(a, b) {
  return a.dev === b.dev && a.ino === b.ino && a.size === b.size
    && a.mtimeMs === b.mtimeMs && a.ctimeMs === b.ctimeMs
    && a.mode === b.mode && a.uid === b.uid && a.gid === b.gid;
}

function acceptedEncoding(header) {
  const choices = new Map();
  for (const part of String(header || '').toLowerCase().split(',')) {
    const [name, ...params] = part.trim().split(';');
    if (!name) continue;
    const q = params.map((item) => item.trim()).find((item) => item.startsWith('q='));
    const quality = q ? Number(q.slice(2)) : 1;
    choices.set(name, Number.isFinite(quality) ? quality : 0);
  }
  const permits = (name) => (choices.get(name) ?? choices.get('*') ?? 0) > 0;
  if (permits('br')) return 'br';
  if (permits('gzip')) return 'gzip';
  return null;
}

export function createStaticServer({ dir, log, sourceRevision = '' } = {}) {
  if (!dir) throw new TypeError('createStaticServer: dir is required');
  const root = path.resolve(dir);
  const index = path.join(root, 'index.html');
  let preparedIndex = null;
  let preparation;

  // Only the small entry document is prepared. Every request still checks its
  // identity; a source replacement falls back to the ordinary streaming path.
  function prepare() {
    return preparation ??= (async () => {
      let file;
      try {
        const directory = await stat(root);
        file = await open(index, fsConstants.O_RDONLY | fsConstants.O_NOFOLLOW | fsConstants.O_NONBLOCK);
        const before = await file.stat();
        if (!before.isFile() || before.size > MAX_PREPARED_INDEX_BYTES) return;
        const buffer = Buffer.alloc(MAX_PREPARED_INDEX_BYTES + 1);
        let length = 0;
        while (length < buffer.length) {
          const { bytesRead } = await file.read(buffer, length, buffer.length - length, length);
          if (!bytesRead) break;
          length += bytesRead;
        }
        if (length !== before.size || length > MAX_PREPARED_INDEX_BYTES || !sameFile(before, await file.stat())) return;
        const body = Buffer.from(buffer.subarray(0, length));
        const variants = { identity: body };
        if (length >= MIN_COMPRESSION_BYTES) {
          [variants.br, variants.gzip] = await Promise.all([
            compressBrotli(body, { params: { [zlibConstants.BROTLI_PARAM_QUALITY]: 5 } }),
            compressGzip(body, { level: 6 }),
          ]);
        }
        if (Object.values(variants).reduce((total, value) => total + value.length, 0) > 3 * MAX_PREPARED_INDEX_BYTES) return;
        preparedIndex = { directory, file: before, variants };
      } catch (error) {
        if (!['ENOENT', 'ENOTDIR', 'ELOOP'].includes(error.code)) throw error;
      } finally {
        await file?.close();
      }
    })();
  }

  function sendText(res, status, body, extraHeaders = {}) {
    const buf = Buffer.from(body, 'utf8');
    if (res.headersSent) {
      res.destroy();
      return;
    }
    res.writeHead(status, {
      'content-type': 'text/plain; charset=utf-8',
      'content-length': buf.length,
      'x-content-type-options': 'nosniff',
      'cache-control': 'no-store',
      ...extraHeaders,
    });
    res.end(buf);
  }

  async function handle(req, res) {
    try {
      const method = req.method || 'GET';
      if (method !== 'GET' && method !== 'HEAD') {
        sendText(res, 405, 'method not allowed', { allow: 'GET, HEAD' });
        return;
      }

      let pathname;
      try {
        pathname = decodeURIComponent(new URL(req.url || '/', 'http://localhost').pathname);
      } catch {
        sendText(res, 400, 'bad request');
        return;
      }
      if (pathname.includes('\0')) {
        sendText(res, 400, 'bad request');
        return;
      }
      if (pathname === '/') pathname = '/index.html';

      // Traversal guard: resolve relative to the root and require the result
      // to stay under it. The leading '.' keeps absolute-looking paths inside.
      const abs = path.resolve(root, '.' + pathname);
      if (abs !== root && !abs.startsWith(root + path.sep)) {
        sendText(res, 404, 'not found');
        return;
      }

      const base = path.basename(abs);
      const ext = path.extname(base).toLowerCase();
      // Only the known asset types are ever served; dotfiles never are.
      if (base.startsWith('.') || !MIME.has(ext)) {
        sendText(res, 404, 'not found');
        return;
      }

      let st;
      try {
        st = await stat(abs);
      } catch (err) {
        if (err.code === 'ENOENT' || err.code === 'ENOTDIR' || err.code === 'EACCES') {
          sendText(res, 404, 'not found');
          return;
        }
        throw err;
      }
      if (!st.isFile()) {
        sendText(res, 404, 'not found');
        return;
      }

      let prepared = null;
      if (abs === index && preparedIndex) {
        const directory = await stat(root);
        if (sameFile(st, preparedIndex.file)
            && directory.dev === preparedIndex.directory.dev && directory.ino === preparedIndex.directory.ino) {
          prepared = preparedIndex;
        } else {
          preparedIndex = null;
        }
      }

      const encoding = COMPRESSIBLE.has(ext) && st.size >= MIN_COMPRESSION_BYTES
        ? acceptedEncoding(req.headers['accept-encoding'])
        : null;
      const encodingTag = encoding ? `-${encoding}` : '';
      const etag = `"${Math.round(st.mtimeMs).toString(16)}-${st.size.toString(16)}${encodingTag}"`;
      const headers = {
        'content-type': MIME.get(ext),
        etag,
        'cache-control': 'no-cache',
        'x-content-type-options': 'nosniff',
      };
      if (sourceRevision) headers['x-ui-source-revision'] = sourceRevision;
      if (COMPRESSIBLE.has(ext)) headers.vary = 'Accept-Encoding';
      if (encoding) headers['content-encoding'] = encoding;

      const inm = req.headers['if-none-match'];
      if (inm && inm.split(',').some((t) => {
        const tag = t.trim();
        return tag === etag || tag === 'W/' + etag;
      })) {
        res.writeHead(304, headers);
        res.end();
        return;
      }

      if (!encoding) headers['content-length'] = st.size;
      if (method === 'HEAD') {
        res.writeHead(200, headers);
        res.end();
        return;
      }

      res.writeHead(200, headers);
      if (prepared) {
        res.end(prepared.variants[encoding || 'identity']);
        return;
      }
      const stream = createReadStream(abs);
      if (encoding === 'br') {
        await pipeline(stream, createBrotliCompress({
          params: { [zlibConstants.BROTLI_PARAM_QUALITY]: 5 },
        }), res);
      } else if (encoding === 'gzip') {
        await pipeline(stream, createGzip({ level: 6 }), res);
      } else {
        await pipeline(stream, res);
      }
    } catch (err) {
      log?.error?.('static handler failure', { error: err?.message });
      if (!res.headersSent) sendText(res, 500, 'internal error');
      else res.destroy();
    }
  }

  return { handle, prepare };
}
