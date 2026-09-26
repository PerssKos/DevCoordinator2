// Report likely untranslated product strings without treating technical identifiers as errors.
// Structural parity remains the admission gate; this audit provides bounded linguistic-review input.
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../../', import.meta.url));
const localeRoot = path.join(root, 'console/locales');
const manifest = JSON.parse(await fs.readFile(path.join(localeRoot, 'manifest.json')));
const requested = process.argv.includes('--locale') ? process.argv[process.argv.indexOf('--locale') + 1] : null;
const strict = process.argv.includes('--strict');
const includeDrafts = process.argv.includes('--all');
const sourceEntry = manifest.locales.find(entry => entry.tag === manifest.sourceLocale);
const technical = new Set([
  'API', 'CPU', 'CSS', 'DOM', 'Docker', 'DevCoordinator', 'Git', 'HTML', 'HTTP', 'JSON',
  'OAuth', 'Playwright', 'Rust', 'SHA-256', 'TTL', 'URL', 'UTC', 'UUID', 'WebSocket',
]);
const readCatalog = async entry => {
  const result = {};
  for (const [namespace, files] of Object.entries(entry.files)) {
    const merged = {};
    for (const file of files) Object.assign(merged, JSON.parse(await fs.readFile(path.join(localeRoot, file), 'utf8')));
    result[namespace] = merged;
  }
  return result;
};
const source = await readCatalog(sourceEntry);
const isTechnical = value => {
  if (typeof value !== 'string') return true;
  const text = value.replace(/\{[A-Za-z][A-Za-z0-9_]*\}/g, '').trim();
  if (!text || text.length <= 7) return true;
  if (/^[A-Z0-9 _./:+-]+$/.test(text)) return true;
  const words = text.split(/\s+/).filter(Boolean);
  return words.length === 1 && technical.has(text);
};
const exact = (sourceValue, translatedValue) => {
  if (typeof sourceValue === 'string') return translatedValue === sourceValue && !isTechnical(sourceValue);
  if (!sourceValue || typeof translatedValue !== 'object') return false;
  return Object.entries(sourceValue.forms || {}).some(([form, text]) => translatedValue.forms?.[form] === text && !isTechnical(text));
};
const entries = requested
  ? manifest.locales.filter(entry => entry.tag === requested)
  : manifest.locales.filter(entry => entry.tag !== manifest.sourceLocale && (includeDrafts || entry.status === 'enabled'));
const reports = [];
for (const entry of entries) {
  const catalog = await readCatalog(entry);
  const matches = [];
  for (const [namespace, messages] of Object.entries(source)) for (const [id, sourceValue] of Object.entries(messages)) {
    if (exact(sourceValue, catalog[namespace]?.[id])) matches.push({ namespace, id, source: typeof sourceValue === 'string' ? sourceValue : sourceValue.forms });
  }
  reports.push({ locale: entry.tag, status: entry.status, exactEnglishProductStrings: matches.length, matches });
}
console.log(JSON.stringify({ sourceLocale: manifest.sourceLocale, locales: reports }, null, 2));
if (strict && reports.some(report => report.exactEnglishProductStrings)) process.exitCode = 1;
