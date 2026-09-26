// Catalog admission is independent of the UI. Draft catalogs are reported, never
// silently treated as translated. Run with --all to require the whole rollout.
import fs from 'node:fs/promises';
import { duplicateKeys } from './json-members.mjs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { messageParameters, canonicalLocale, intlLocales } from '../../console/i18n-core.mjs';
const root = fileURLToPath(new URL('../../', import.meta.url));
const dir = path.join(root, 'console/locales');
const manifest = JSON.parse(await fs.readFile(path.join(dir, 'manifest.json')));
const errors = [], drafts = [], namespaces = new Map();
const localeArgument = process.argv.indexOf('--locale');
const requestedLocale = localeArgument === -1 ? null : process.argv[localeArgument + 1];
const tags = new Set();
const check = (condition, message) => { if (!condition) errors.push(message); };
async function read(entry, namespace) {
  const all = Object.create(null);
  const files = entry.files[namespace];
  check(Array.isArray(files) && files.length > 0, `${entry.tag}: missing namespace ${namespace}`);
  for (const file of files || []) {
    check(file.startsWith(`${entry.tag}/`) && !file.split('/').includes('..') && file.endsWith('.json'), `${entry.tag}: invalid file mapping ${file}`);
    if (!file.startsWith(`${entry.tag}/`) || file.split('/').includes('..')) continue;
    let values;
    try {
      const json = await fs.readFile(path.join(dir, file), 'utf8');
      values = JSON.parse(json);
      for (const key of duplicateKeys(json)) errors.push(`${file}: duplicate member ${key}`);
    }
    catch { errors.push(`${file}: missing or invalid JSON`); continue; }
    if (!values || typeof values !== 'object' || Array.isArray(values)) { errors.push(`${file}: expected messages object`); continue; }
    for (const [key, value] of Object.entries(values)) {
      check(!Object.hasOwn(all,key), `${file}: duplicate ${key}`);
      check(typeof value === 'string' || value && typeof value.argument === 'string' && value.forms && typeof value.forms.other === 'string', `${file}: invalid ${key}`);
      const patterns = typeof value === 'string' ? [value] : Object.values(value?.forms || {});
      check(patterns.length > 0 && patterns.every(text => typeof text === 'string' && text.trim() && !/<\/?[a-z][a-z0-9-]*(?:\s[^>]*)?>/i.test(text)), `${file}: empty or HTML message ${key}`);
      if (typeof value === 'object' && value?.forms) {
        const categories = new Intl.PluralRules(intlLocales(entry.tag, manifest), { type: value.type || 'cardinal' }).resolvedOptions().pluralCategories;
        check(categories.every(category => Object.hasOwn(value.forms,category)), `${file}: missing plural category ${key}`);
      }
      all[key] = value;
    }
  }
  return all;
}
const source = manifest.locales.find(entry => entry.tag === manifest.sourceLocale);
if (localeArgument !== -1) check(manifest.locales.some(entry => entry.tag === requestedLocale), 'Unknown --locale');
check(source?.status === 'enabled','Source locale must be enabled');
for (const ns of manifest.namespaces) namespaces.set(ns, await read(source,ns));
for (const entry of manifest.locales) {
  check(!tags.has(entry.tag), `Duplicate locale ${entry.tag}`); tags.add(entry.tag);
  check(['ltr','rtl'].includes(entry.direction), `${entry.tag}: invalid direction`);
  check(entry.intlFallbacks === undefined || (Array.isArray(entry.intlFallbacks) && entry.intlFallbacks.every(tag => typeof tag === 'string' && canonicalLocale(tag))), `${entry.tag}: invalid Intl fallback list`);
  check(entry.nativeName && entry.englishName, `${entry.tag}: missing display names`);
  check(['draft','enabled'].includes(entry.status), `${entry.tag}: invalid status`);
  check(entry.countries.length >= 1 && entry.countries.length <= 3 && new Set(entry.countries).size === entry.countries.length, `${entry.tag}: flag count`);
  for (const code of entry.countries) { try { await fs.access(path.join(root, 'console/icons/flags', code.toLowerCase()+'.svg')); } catch { errors.push(`${entry.tag}: missing flag ${code}`); } }
  if (entry.status !== 'enabled') { drafts.push(entry.tag); if (!process.argv.includes('--all') && entry.tag !== requestedLocale) continue; }
  for (const ns of manifest.namespaces) {
    const values = entry === source ? namespaces.get(ns) : await read(entry, ns);
    const en = namespaces.get(ns);
    for (const [key,message] of Object.entries(en)) {
      check(Object.hasOwn(values,key), `${entry.tag}/${ns}: missing ${key}`);
      if (values[key] != null) check(JSON.stringify(messageParameters(message)) === JSON.stringify(messageParameters(values[key])), `${entry.tag}/${ns}: arguments differ for ${key}`);
      if (typeof message === 'object') check(typeof values[key] === 'object', `${entry.tag}/${ns}: missing plural forms for ${key}`);
    }
    for (const key of Object.keys(values)) check(Object.hasOwn(en,key), `${entry.tag}/${ns}: unknown ${key}`);
  }
}
console.log(JSON.stringify({ enabled:manifest.locales.length-drafts.length, drafts, sourceMessages:[...namespaces.values()].reduce((n,v)=>n+Object.keys(v).length,0), errors },null,2));
process.exitCode = errors.length ? 1 : 0;
