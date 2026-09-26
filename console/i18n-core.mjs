// Shared by the static Console and the edge. No DOM, network, or Node dependencies.
export function canonicalLocale(value) {
  try { return new Intl.Locale(String(value)).baseName; } catch { return null; }
}

// Different Intl services may support different language sets. Keep a declared
// fallback order instead of inheriting the browser or server's default locale.
export function intlLocales(locale, manifest) {
  const declared = manifest.locales.find(entry => entry.tag === locale)?.intlFallbacks;
  const fallbacks = Array.isArray(declared) ? declared.filter(tag => typeof tag === 'string' && canonicalLocale(tag)) : [];
  return [...new Set([locale, ...fallbacks, manifest.sourceLocale])];
}

export function matchLocale(value, locales) {
  const tag = canonicalLocale(value);
  if (!tag) return null;
  const enabled = locales.filter(entry => entry.status === 'enabled');
  const exact = enabled.find(entry => canonicalLocale(entry.tag) === tag || entry.aliases?.some(alias => canonicalLocale(alias) === tag));
  if (exact) return exact.tag;
  const requested = new Intl.Locale(tag);
  // Keep scripts distinct, notably zh-TW/zh-HK versus zh-CN and Serbian scripts.
  const script = requested.maximize().script;
  const alias = enabled.find(entry => entry.aliases?.includes(requested.language));
  if (alias) return alias.tag;
  const candidates = enabled.filter(entry => new Intl.Locale(entry.tag).language === requested.language);
  const regional = candidates.find(entry => new Intl.Locale(entry.tag).region === requested.region && new Intl.Locale(entry.tag).maximize().script === script);
  if (requested.region && regional) return regional.tag;
  return candidates.find(entry => new Intl.Locale(entry.tag).maximize().script === script)?.tag || null;
}

export function browserLocales(preferences, locales) {
  return [...new Set((preferences || []).map(tag => matchLocale(tag, locales)).filter(Boolean))];
}

export function recentLocales(recent, preferences, locales) {
  const enabled = new Set(locales.filter(entry => entry.status === 'enabled').map(entry => entry.tag));
  return [...new Set([...[...new Set((recent || []).filter(tag => enabled.has(tag)))].slice(0, 5), ...browserLocales(preferences, locales)])];
}

export function negotiateLocale(preference, preferences, manifest) {
  return matchLocale(preference, manifest.locales) || browserLocales(preferences, manifest.locales)[0] || manifest.sourceLocale;
}

export function acceptLanguages(header = '') {
  return String(header).split(',').map((part, index) => {
    const [tag, ...parameters] = part.trim().split(';');
    const q = parameters.find(p => p.trim().startsWith('q='));
    return { tag, quality: q ? Number(q.trim().slice(2)) : 1, index };
  }).filter(entry => Number.isFinite(entry.quality) && entry.quality > 0 && entry.quality <= 1)
    .sort((a, b) => b.quality - a.quality || a.index - b.index).map(entry => entry.tag);
}

export function localeCookie(header = '') {
  const value = String(header).split(';').map(p => p.trim()).find(p => p.startsWith('dc2-locale='));
  try { return value ? canonicalLocale(decodeURIComponent(value.slice(11))) : null; } catch { return null; }
}

export function formatMessage(message, params = {}, locale = 'en') {
  let pattern = message;
  if (message && typeof message === 'object') {
    const count = Number(params[message.argument]);
    const category = new Intl.PluralRules(locale, { type: message.type || 'cardinal' }).select(count);
    pattern = message.forms[`=${count}`] ?? message.forms[category] ?? message.forms.other;
  }
  if (typeof pattern !== 'string') throw new TypeError('Invalid message');
  return pattern.replace(/\{([A-Za-z][A-Za-z0-9_]*)\}/g, (_match, name) => {
    if (!Object.hasOwn(params, name)) throw new TypeError(`Missing message argument: ${name}`);
    return typeof params[name] === 'number' ? new Intl.NumberFormat(locale).format(params[name]) : String(params[name]);
  });
}

export function messageParameters(message) {
  const strings = typeof message === 'string' ? [message] : Object.values(message.forms);
  return [...new Set([...(typeof message === 'object' ? [message.argument] : []), ...strings.flatMap(text => [...text.matchAll(/\{([A-Za-z][A-Za-z0-9_]*)\}/g)].map(m => m[1]))])].sort();
}

export function createCatalog(manifest, read) {
  const cache = new Map();
  const inflight = new Map();
  async function load(locale, namespace) {
    const key = `${locale}/${namespace}`;
    if (cache.has(key)) return cache.get(key);
    if (inflight.has(key)) return inflight.get(key);
    const files = manifest.locales.find(entry => entry.tag === locale)?.files?.[namespace];
    if (!files?.length) throw new Error(`Catalog namespace unavailable: ${key}`);
    const promise = Promise.all(files.map(file => read(file))).then(parts => {
      const merged = Object.create(null);
      for (const part of parts) for (const [id, message] of Object.entries(part)) {
        if (Object.hasOwn(merged, id)) throw new Error(`Duplicate message: ${key}.${id}`);
        merged[id] = message;
      }
      cache.set(key, merged);
      return merged;
    }).finally(() => inflight.delete(key));
    inflight.set(key, promise);
    return promise;
  }
  async function ensure(locale, namespaces) {
    const failures = [];
    await Promise.all([...new Set(namespaces)].map(async namespace => {
      await load(manifest.sourceLocale, namespace);
      if (locale !== manifest.sourceLocale) {
        try { await load(locale, namespace); } catch { failures.push(namespace); }
      }
    }));
    return failures;
  }
  function message(locale, id, params = {}) {
    const dot = id.indexOf('.'); const namespace = id.slice(0, dot); const key = id.slice(dot + 1);
    const source = cache.get(`${manifest.sourceLocale}/${namespace}`)?.[key];
    const translated = cache.get(`${locale}/${namespace}`)?.[key];
    if (source == null) throw new Error(`Unknown message: ${id}`);
    try { return formatMessage(translated ?? source, params, intlLocales(translated == null ? manifest.sourceLocale : locale, manifest)); }
    catch { return formatMessage(source, params, intlLocales(manifest.sourceLocale, manifest)); }
  }
  function sourceKey(namespace, value, prefix = '') {
    return Object.entries(cache.get(`${manifest.sourceLocale}/${namespace}`) || {}).find(([key, text]) => key.startsWith(prefix) && text === value)?.[0] ?? null;
  }
  return { ensure, message, sourceKey };
}
