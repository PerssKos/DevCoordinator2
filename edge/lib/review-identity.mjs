// Optional compatibility bridge for an already-authorized review application.
// Private trust material never enters the public route document or edge logs.
import fs from 'node:fs';
import path from 'node:path';
import { createPrivateKey, randomBytes, sign } from 'node:crypto';

export const REVIEW_IDENTITY_HEADER = 'x-spectre-review-identity';
const CONFIG_ERROR = 'review identity: cannot read or validate private configuration';
const ASSERTION_ERROR = 'review identity: verified request identity is unavailable';
const CONTROL_RE = /[\u0000-\u001f\u007f]/;
const LABEL_RE = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const ID_RE = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;
const KID_RE = /^[A-Za-z0-9._-]{1,64}$/;
const AUDIENCE_RE = /^route:[A-Za-z0-9][A-Za-z0-9._:-]{7,255}$/;
const THREAD_RE = /^\/api\/help\/reviews\/[0-9a-f]{32}(?:\/(?:messages|position|decision))?$/;
const MEDIA_RE = /^\/api\/help\/review-media\/[0-9a-f]{32}\/content$/;
const EXACT_PATHS = new Set([
  '/api/help/review-context', '/api/help/review-whoami', '/api/help/reviews', '/api/help/review-media',
  '/api/admin/help/reviews/export',
]);

function exactFields(value, fields) {
  return value && typeof value === 'object' && !Array.isArray(value)
    && Object.keys(value).length === fields.length
    && fields.every((field) => Object.hasOwn(value, field));
}

function boundedText(value, maximum) {
  return typeof value === 'string' && value.length > 0 && value.length <= maximum
    && !CONTROL_RE.test(value);
}

function readPrivateFile(file, maximum) {
  if (typeof file !== 'string' || !path.isAbsolute(file)) throw new Error(CONFIG_ERROR);
  const descriptor = fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW);
  try {
    const info = fs.fstatSync(descriptor);
    const uid = typeof process.getuid === 'function' ? process.getuid() : info.uid;
    if (!info.isFile() || info.nlink !== 1 || (info.uid !== 0 && info.uid !== uid)
      || (info.mode & 0o077) !== 0 || info.size < 1 || info.size > maximum) throw new Error(CONFIG_ERROR);
    const content = fs.readFileSync(descriptor);
    if (content.length !== info.size) throw new Error(CONFIG_ERROR);
    return content;
  } finally {
    fs.closeSync(descriptor);
  }
}

export function isReviewIdentityTarget(target) {
  if (!boundedText(target, 4096) || !target.startsWith('/') || target.includes('#')) return false;
  const pathname = target.split('?', 1)[0];
  return EXACT_PATHS.has(pathname) || THREAD_RE.test(pathname) || MEDIA_RE.test(pathname);
}

export function loadReviewIdentity(file, { oidcIssuer } = {}) {
  if (!file) return { assertionFor: () => null };
  let document;
  let privateKey;
  const routes = new Map();
  try {
    document = JSON.parse(readPrivateFile(file, 65536).toString('utf8'));
    if (!exactFields(document, ['schema', 'issuer', 'identity_provider_issuer', 'kid', 'private_key_file', 'routes'])
      || document.schema !== 1 || !boundedText(document.issuer, 512)
      || !boundedText(document.identity_provider_issuer, 512)
      || document.identity_provider_issuer !== oidcIssuer
      || !boundedText(document.kid, 64) || !KID_RE.test(document.kid) || !Array.isArray(document.routes)
      || document.routes.length < 1 || document.routes.length > 32) throw new Error(CONFIG_ERROR);
    for (const route of document.routes) {
      if (!exactFields(route, ['label', 'deployment_id', 'component', 'audience'])
        || !boundedText(route.label, 63) || !LABEL_RE.test(route.label)
        || !boundedText(route.deployment_id, 128) || !ID_RE.test(route.deployment_id)
        || !boundedText(route.component, 128) || !ID_RE.test(route.component)
        || !boundedText(route.audience, 262) || !AUDIENCE_RE.test(route.audience)
        || routes.has(route.label)) throw new Error(CONFIG_ERROR);
      routes.set(route.label, { ...route });
    }
    privateKey = createPrivateKey(readPrivateFile(document.private_key_file, 16384));
    if (privateKey.asymmetricKeyType !== 'ed25519') throw new Error(CONFIG_ERROR);
  } catch {
    // Never disclose private file paths, contents, parser diagnostics or keys.
    throw new Error(CONFIG_ERROR);
  }

  return {
    assertionFor({ route, identity, method, target } = {}) {
      const binding = routes.get(route?.label);
      if (!binding || route.auth !== 'authenticated'
        || route.deployment_id !== binding.deployment_id || route.component !== binding.component
        || !isReviewIdentityTarget(target)) return null;
      if (!boundedText(identity?.sub, 512) || !boundedText(identity?.email, 254)
        || !/^[^@\s]+@[^@\s]+$/.test(identity.email)
        || typeof method !== 'string' || !/^[A-Z]{1,16}$/.test(method)) throw new Error(ASSERTION_ERROR);
      const name = identity.name || identity.email;
      if (!boundedText(name, 120)) throw new Error(ASSERTION_ERROR);
      const iat = Math.floor(Date.now() / 1000);
      const header = { alg: 'EdDSA', kid: document.kid, typ: 'spectre-review-identity+jwt', v: 1 };
      const payload = {
        v: 1, iss: document.issuer, idp: document.identity_provider_issuer,
        sub: identity.sub, email: identity.email.toLowerCase(), name,
        aud: binding.audience, method, target, iat, exp: iat + 30,
        jti: randomBytes(18).toString('base64url'),
      };
      const body = `${Buffer.from(JSON.stringify(header)).toString('base64url')}.${Buffer.from(JSON.stringify(payload)).toString('base64url')}`;
      return `${body}.${sign(null, Buffer.from(body, 'ascii'), privateKey).toString('base64url')}`;
    },
  };
}
