#!/usr/bin/env node
// DevCoordinator2 stable edge: public TLS, sign-in and sessions, per-route
// deployment-grant enforcement, domain→port proxying from the last valid
// atomic route document, and availability while the daemon restarts.
// It owns nothing else (no lifecycle, no test state, no users as a second
// authority): users and grants come from the route document the daemon
// publishes; the daemon is asked only to admit an invited identity.
//
// Configuration is instance data: environment variables (see
// docs/edge.md), typically from /etc/devcoordinator2/edge.env and systemd
// credentials. No value here names any installation.

import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import { URL, fileURLToPath } from 'node:url';

import { createDaemonClient } from './lib/daemon-client.mjs';
import { createOidc } from './lib/oidc.mjs';
import { createPages } from './lib/pages.mjs';
import { createProxy } from './lib/proxy.mjs';
import { createRoutesStore } from './lib/routes-store.mjs';
import { createSessionManager, parseCookies } from './lib/session.mjs';
import { createStaticServer } from './lib/static.mjs';

const SESSION_COOKIE = 'dc2_session';
const SESSION_TTL_MS = 12 * 60 * 60 * 1000;
const ROLE_RANK = { access: 0, viewer: 1, operator: 2, administrator: 3 };

function env(name, fallback = '') {
  const value = process.env[name];
  return value === undefined || value === '' ? fallback : value;
}

function readSecretFile(file, label) {
  if (!file) return '';
  try {
    return fs.readFileSync(file, 'utf8').trim();
  } catch (error) {
    throw new Error(`${label}: cannot read ${file}: ${error.message}`);
  }
}

export function loadConfig(e = process.env) {
  const baseDomain = (e.EDGE_BASE_DOMAIN || '').trim().replace(/\.$/, '');
  if (!baseDomain) throw new Error('EDGE_BASE_DOMAIN is required');
  const httpOnly = e.EDGE_HTTP_ONLY === '1';
  return {
    baseDomain,
    consoleHost: (e.EDGE_CONSOLE_HOST || `console.${baseDomain}`).toLowerCase(),
    httpPort: Number(e.EDGE_HTTP_PORT || (httpOnly ? 8080 : 80)),
    httpsPort: Number(e.EDGE_HTTPS_PORT || 443),
    httpOnly,
    tlsCert: e.EDGE_TLS_CERT || '',
    tlsKey: e.EDGE_TLS_KEY || '',
    sessionSecret: readSecretFile(e.EDGE_SESSION_SECRET_FILE, 'session secret')
      || e.EDGE_SESSION_SECRET || '',
    oidcIssuer: e.EDGE_OIDC_ISSUER || 'https://accounts.google.com',
    oidcClientId: readSecretFile(e.EDGE_OIDC_CLIENT_ID_FILE, 'oidc client id') || e.EDGE_OIDC_CLIENT_ID || '',
    oidcClientSecret: readSecretFile(e.EDGE_OIDC_CLIENT_SECRET_FILE, 'oidc client secret') || e.EDGE_OIDC_CLIENT_SECRET || '',
    routesFile: e.EDGE_ROUTES_FILE || '/var/lib/devcoordinator2/routes.json',
    stateDir: e.EDGE_STATE_DIR || '/var/lib/devcoordinator2-edge',
    daemonSocket: e.EDGE_DAEMON_SOCKET || '/run/devcoordinator2/daemon.sock',
    consoleDir: e.EDGE_CONSOLE_DIR || '',
  };
}

function hostOf(req) {
  const raw = String(req.headers.host || '').toLowerCase();
  if (!raw || raw.length > 300 || /[\s\r\n]/.test(raw)) return '';
  return raw.replace(/:\d+$/, '');
}

function writeJson(res, status, value, headers = {}) {
  const body = Buffer.from(`${JSON.stringify(value)}\n`);
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store',
    'x-content-type-options': 'nosniff', 'content-length': body.length, ...headers });
  res.end(body);
}

function writePage(res, page, headers = {}) {
  const body = Buffer.from(page.html);
  res.writeHead(page.status ?? 200, { 'content-type': 'text/html; charset=utf-8',
    'cache-control': 'no-store', 'content-length': body.length, ...headers });
  res.end(body);
}

function redirect(res, location, headers = {}) {
  res.writeHead(302, { location, 'cache-control': 'no-store', 'content-length': '0', ...headers });
  res.end();
}

export function authorize(doc, route, identity) {
  if (route.auth === 'public') return { allowed: true, role: 'public' };
  if (!identity) return { allowed: false, reason: 'sign-in required' };
  if (doc.access.owners.includes(identity)) return { allowed: true, role: 'administrator' };
  const grant = doc.access.grants.find((g) => g.identity === identity && g.deployment_id === route.deployment_id);
  if (!grant || ROLE_RANK[grant.role] === undefined) return { allowed: false, reason: 'no grant for this deployment' };
  return { allowed: true, role: grant.role };
}

async function readJsonBody(req, limit = 65536) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (c) => { size += c.length; if (size > limit) { reject(new Error('body too large')); req.destroy(); } else chunks.push(c); });
    req.on('end', () => {
      if (!chunks.length) return resolve({});
      try { resolve(JSON.parse(Buffer.concat(chunks).toString('utf8'))); } catch (e) { reject(e); }
    });
    req.on('error', reject);
  });
}

export async function createEdge(config, { log = console } = {}) {
  const scheme = config.httpOnly ? 'http' : 'https';
  const store = await createRoutesStore({ file: config.routesFile, stateDir: config.stateDir, log });
  const sessions = createSessionManager({ secret: config.sessionSecret, ttlMs: SESSION_TTL_MS,
    cookieName: SESSION_COOKIE, cookieDomain: `.${config.baseDomain}`, secure: !config.httpOnly });
  // The console origin (and so the OIDC redirect URI) depends on the bound
  // port in http-only/canary mode; both are finalized in listen().
  let consoleOrigin = `${scheme}://${config.consoleHost}`;
  let pages = createPages({ config: { domain: config.baseDomain, consoleOrigin } });
  let oidc = createOidc({ issuer: config.oidcIssuer, clientId: config.oidcClientId,
    clientSecret: config.oidcClientSecret, redirectUri: `${consoleOrigin}/auth/callback`, sessions, log });
  function finalizeOrigin(boundPort) {
    const defaultPort = config.httpOnly ? 80 : 443;
    consoleOrigin = `${scheme}://${config.consoleHost}${boundPort !== defaultPort ? `:${boundPort}` : ''}`;
    pages = createPages({ config: { domain: config.baseDomain, consoleOrigin } });
    oidc = createOidc({ issuer: config.oidcIssuer, clientId: config.oidcClientId,
      clientSecret: config.oidcClientSecret, redirectUri: `${consoleOrigin}/auth/callback`, sessions, log });
  }
  const proxy = createProxy({ log, sessionCookieName: SESSION_COOKIE,
    renderBadGateway: (req, res, { kind, target }) => writePage(res, pages.renderUpstreamError({ slug: target.slug, kind, consoleUrl: consoleOrigin })),
    renderUpstreamAuthFailure: (req, res, { target }) => writePage(res, pages.renderUpstreamError({ slug: target.slug, kind: 'upstream_auth', consoleUrl: consoleOrigin })) });
  const daemon = createDaemonClient({ socketPath: config.daemonSocket });
  const consoleStatic = config.consoleDir ? createStaticServer({ dir: config.consoleDir, log }) : null;

  function identityOf(req) {
    const session = sessions.parse(req.headers.cookie);
    return session ? { email: session.email, sub: session.sub, name: session.name } : null;
  }

  async function handleAuth(req, res, url, host) {
    const rt = url.searchParams.get('rt') || '/';
    if (url.pathname === '/auth/login') {
      if (identityOf(req)) return redirect(res, rt);
      return writePage(res, pages.renderLogin({ rt, degraded: !oidc.configured }));
    }
    if (url.pathname === '/auth/google' || url.pathname === '/auth/start') {
      if (host !== config.consoleHost) {
        // Sign-in always completes on the console host; remember where to return.
        const back = `${scheme}://${req.headers.host}${rt}`;
        return redirect(res, `${consoleOrigin}/auth/start?rt=${encodeURIComponent(back)}`);
      }
      try {
        const { url: target, flowCookie } = await oidc.loginRedirect(rt);
        return redirect(res, target, { 'set-cookie': flowCookie });
      } catch (error) {
        log.warn?.('sign-in redirect failed', { error: error.message, stack: error.stack });
        return writePage(res, pages.renderLogin({ rt, error: 'sign-in is not available right now', degraded: true }));
      }
    }
    if (url.pathname === '/auth/callback') {
      try {
        const flow = parseCookies(req.headers.cookie).dc_flow;
        const { profile, rt: back } = await oidc.handleCallback(url.searchParams, flow);
        const { cookie } = sessions.issue(profile);
        try {
          await daemon.call('user.accept_invitation', { email: profile.email, subject: String(profile.sub),
            display_name: profile.name || null }, profile.email);
        } catch (error) {
          log.warn?.('daemon unavailable during sign-in; session issued, grants from route document', { error: error.message });
        }
        return redirect(res, back || '/', { 'set-cookie': [cookie, 'dc_flow=; Path=/; Max-Age=0'] });
      } catch (error) {
        log.warn?.('sign-in callback failed', { error: error.message });
        return writePage(res, pages.renderLogin({ rt: '/', error: 'sign-in failed; please try again' }));
      }
    }
    if (url.pathname === '/auth/logout') {
      return redirect(res, '/auth/login', { 'set-cookie': sessions.clearCookie() });
    }
    return writeJson(res, 404, { ok: false, error: { code: 'not_found' } });
  }

  async function handleConsole(req, res, url) {
    if (url.pathname === '/healthz') {
      return writeJson(res, 200, {
        ok: store.rejectedGeneration() === null,
        route_generation: store.current().generation,
        route_schema: store.current().schema,
        route_sha256: store.current().payload_sha256,
        rejected_route_generation: store.rejectedGeneration(),
        source: store.source(),
      });
    }
    if (url.pathname.startsWith('/api/v2/')) {
      const identity = identityOf(req);
      if (!identity) return writeJson(res, 401, { ok: false, error: { code: 'unauthenticated', message: 'sign in first' } });
      if (req.method !== 'POST') return writeJson(res, 405, { ok: false, error: { code: 'method_not_allowed' } });
      const operation = url.pathname.slice('/api/v2/'.length);
      // One or more dot-separated operation segments, plus the daemon's
      // single dotless operation; anything else is grammar garbage and never
      // reaches the daemon.
      if (!/^([a-z]+(?:\.[a-z_]+)+|ping)$/.test(operation)) return writeJson(res, 400, { ok: false, error: { code: 'args_invalid' } });
      let params;
      try { params = await readJsonBody(req); } catch { return writeJson(res, 400, { ok: false, error: { code: 'args_invalid', message: 'invalid JSON body' } }); }
      const cancellation = new AbortController();
      const cancelDisconnectedWait = () => {
        if (!res.writableEnded) cancellation.abort();
      };
      if (operation === 'event.wait') res.once('close', cancelDisconnectedWait);
      try {
        const response = await daemon.call(operation, params, identity.email, { signal: cancellation.signal });
        return writeJson(res, response.ok ? 200 : (response.error?.code === 'permission_denied' ? 403 : 400), response);
      } catch (error) {
        return writeJson(res, 503, { ok: false, error: { code: 'daemon_unavailable', message: error.message } });
      } finally {
        res.removeListener('close', cancelDisconnectedWait);
      }
    }
    if (url.pathname.startsWith('/api/')) {
      return writeJson(res, 400, {
        protocol: 2, id: '', ok: false,
        error: { code: 'protocol_unsupported', message: 'use /api/v2/<operation>', detail: '' },
      });
    }
    const identity = identityOf(req);
    if (!identity) return redirect(res, `/auth/login?rt=${encodeURIComponent(url.pathname)}`);
    if (consoleStatic) {
      if (url.pathname === '/' || url.pathname === '/index.html') req.url = '/index.html';
      return consoleStatic.handle(req, res);
    }
    return writePage(res, { status: 200, html: '<!doctype html><title>DevCoordinator2</title><p>Console assets are not configured on this edge.</p>' });
  }

  async function handleRequest(req, res) {
    const host = hostOf(req);
    const url = new URL(req.url || '/', `${scheme}://${host || config.consoleHost}`);
    if (url.pathname.startsWith('/auth/')) return handleAuth(req, res, url, host);
    if (host === config.consoleHost) return handleConsole(req, res, url);
    const doc = store.current();
    const route = doc.routes.find((r) => r.domain === host);
    if (!route) return writePage(res, pages.renderNotFound({ host }));
    const identity = identityOf(req);
    const decision = authorize(doc, route, identity?.email || null);
    if (!decision.allowed) {
      if (!identity) return redirect(res, `/auth/login?rt=${encodeURIComponent(url.pathname + url.search)}`);
      return writePage(res, pages.renderDenied({ email: identity.email, resource: host, sessionSet: true }));
    }
    return proxy.forward(req, res, target(route, host, identity));
  }

  // Authenticated routes tell the upstream who signed in (verified identity)
  // and which route it came through; public routes stay attribution-free.
  function target(route, host, identity) {
    return { port: route.port, publicHost: host, slug: route.label, route,
      localAttribution: { routeId: `${route.deployment_id}/${route.component}`, email: identity?.email ?? null } };
  }

  function handleUpgrade(req, socket, head) {
    const host = hostOf(req);
    const doc = store.current();
    const route = doc.routes.find((r) => r.domain === host);
    const identity = identityOf(req);
    if (!route || !authorize(doc, route, identity?.email || null).allowed) {
      socket.write('HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n');
      return socket.destroy();
    }
    return proxy.forwardUpgrade(req, socket, head, target(route, host, identity));
  }

  const servers = [];
  if (config.httpOnly) {
    const server = http.createServer((req, res) => { handleRequest(req, res).catch((e) => { log.error?.('request failed', { error: e.message }); if (!res.headersSent) writeJson(res, 500, { ok: false }); }); });
    server.on('upgrade', handleUpgrade);
    servers.push({ server, port: config.httpPort });
  } else {
    const tls = { cert: fs.readFileSync(config.tlsCert), key: fs.readFileSync(config.tlsKey) };
    const secure = https.createServer(tls, (req, res) => { handleRequest(req, res).catch((e) => { log.error?.('request failed', { error: e.message }); if (!res.headersSent) writeJson(res, 500, { ok: false }); }); });
    secure.on('upgrade', handleUpgrade);
    servers.push({ server: secure, port: config.httpsPort });
    const plain = http.createServer((req, res) => redirect(res, `https://${hostOf(req) || config.consoleHost}${req.url}`));
    servers.push({ server: plain, port: config.httpPort });
  }

  async function listen() {
    for (const entry of servers) {
      await new Promise((resolve, reject) => entry.server.listen(entry.port, (err) => (err ? reject(err) : resolve())));
      entry.bound = entry.server.address().port;
    }
    finalizeOrigin(servers[0].bound);
    return servers.map((s) => s.bound);
  }

  async function close() {
    store.close();
    proxy.close();
    await Promise.all(servers.map((s) => new Promise((resolve) => {
      s.server.close(resolve);
      s.server.closeAllConnections?.();
    })));
  }

  return { listen, close, store, handleRequest, get consoleOrigin() { return consoleOrigin; } };
}

const invokedDirectly = (() => { try { return fs.realpathSync(process.argv[1] || '') === fs.realpathSync(fileURLToPath(import.meta.url)); } catch { return false; } })();
if (invokedDirectly) {
  const config = loadConfig();
  const edge = await createEdge(config);
  const ports = await edge.listen();
  console.log(JSON.stringify({ event: 'edge.listening', ports, console: edge.consoleOrigin }));
  const stop = () => edge.close().then(() => process.exit(0));
  process.on('SIGTERM', stop);
  process.on('SIGINT', stop);
}
