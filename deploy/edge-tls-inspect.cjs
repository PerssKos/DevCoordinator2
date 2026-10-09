// Embedded in the commit-stamped maintenance binary. Never print PEM or errors.
const fs = require('node:fs');
const { X509Certificate, createPrivateKey } = require('node:crypto');
const { createSecureContext } = require('node:tls');
try {
  const [certPath, keyPath, hostsJson, current] = process.argv.slice(1);
  const certBytes = fs.readFileSync(certPath);
  const keyBytes = fs.readFileSync(keyPath);
  const cert = new X509Certificate(certBytes);
  if (!cert.checkPrivateKey(createPrivateKey(keyBytes))) throw new Error();
  createSecureContext({ cert: certBytes, key: keyBytes });
  const validFrom = Date.parse(cert.validFrom);
  const validTo = Date.parse(cert.validTo);
  const currentlyValid = Number.isFinite(validFrom) && Number.isFinite(validTo)
    && validFrom <= Date.now() && validTo > Date.now();
  if (current !== 'current') {
    if (!currentlyValid) throw new Error();
    for (const host of JSON.parse(hostsJson)) {
      // Require wildcard coverage itself, rather than one illustrative host.
      const wildcard = host.startsWith('*.');
      const match = cert.checkHost(wildcard ? `renewal-check.${host.slice(2)}` : host,
        { subject: 'never' });
      if (!match || (wildcard && match.toLowerCase() !== host.toLowerCase())) throw new Error();
    }
  }
  process.stdout.write(JSON.stringify({ fingerprint: cert.fingerprint256.replaceAll(':', '').toLowerCase(),
    expires_at_ms: validTo, currently_valid: currentlyValid }));
} catch {
  process.exitCode = 1;
}
