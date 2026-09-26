import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { acceptLanguages, localeCookie, negotiateLocale, formatMessage, intlLocales } from '../../console/i18n-core.mjs';

// Public product copy only. No instance values, auth records or user content enter catalogs.
export function createPageLocalization(directory = fileURLToPath(new URL('../../console/locales/', import.meta.url))) {
  const manifest = JSON.parse(readFileSync(path.join(directory, 'manifest.json'), 'utf8'));
  const cache = new Map();
  const read = locale => {
    if (cache.has(locale)) return cache.get(locale);
    const messages = Object.create(null);
    for (const file of manifest.locales.find(entry => entry.tag === locale)?.files.auth || []) {
      // Paths come from the source-owned manifest, never a request or cookie.
      Object.assign(messages, JSON.parse(readFileSync(path.join(directory, file), 'utf8')));
    }
    cache.set(locale, messages); return messages;
  };
  const source = read(manifest.sourceLocale);
  function forRequest(request) {
    const locale = negotiateLocale(localeCookie(request?.headers?.cookie), acceptLanguages(request?.headers?.['accept-language']), manifest);
    let messages;
    try { messages = read(locale); } catch { messages = Object.create(null); }
    return { locale, direction: manifest.locales.find(entry => entry.tag === locale).direction, t(id, params = {}) {
      if (!Object.hasOwn(source, id)) throw new Error(`Unknown auth message: ${id}`);
      try { return formatMessage(messages[id] ?? source[id], params, intlLocales(messages[id] == null ? manifest.sourceLocale : locale, manifest)); }
      catch { return formatMessage(source[id], params, intlLocales(manifest.sourceLocale, manifest)); }
    } };
  }
  return { forRequest };
}
