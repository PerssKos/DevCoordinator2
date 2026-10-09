// A normal CA-verified TLS request with the exact candidate leaf fingerprint.
const https = require('node:https');
const [host, port, fingerprint] = process.argv.slice(1);
const deadline = setTimeout(() => process.exit(1), 10000);
const req = https.get({ hostname: '127.0.0.1', port: Number(port), servername: host,
  path: '/healthz', headers: { host }, agent: false }, (res) => {
  const peer = res.socket.getPeerCertificate();
  let bytes = 0; const chunks = [];
  res.on('data', chunk => {
    bytes += chunk.length;
    if (bytes > 16384) { req.destroy(); process.exitCode = 1; }
    else chunks.push(chunk);
  });
  res.on('end', () => {
    clearTimeout(deadline);
    try {
      const body = JSON.parse(Buffer.concat(chunks).toString());
      if (res.statusCode !== 200 || body.ok !== true
        || peer.fingerprint256?.replaceAll(':', '').toLowerCase() !== fingerprint) throw new Error();
      process.stdout.write('{"verified":true}');
    } catch { process.exitCode = 1; }
  });
  res.on('error', () => { clearTimeout(deadline); process.exitCode = 1; });
});
req.on('error', () => { clearTimeout(deadline); process.exitCode = 1; });
req.setTimeout(5000, () => req.destroy());
