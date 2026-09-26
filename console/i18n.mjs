import { createCatalog, negotiateLocale, recentLocales, browserLocales, matchLocale, localeCookie, intlLocales } from './i18n-core.mjs';
const read = async file => {
  const response = await fetch(`/locales/${file}`, { cache: 'no-cache' });
  if (!response.ok) throw new Error('Language file unavailable');
  return response.json();
};
const escape = value => String(value ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const stored = (key, fallback = null) => { try { return localStorage.getItem(key) ?? fallback; } catch { return fallback; } };
const save = (key, value) => { try { value == null ? localStorage.removeItem(key) : localStorage.setItem(key, value); } catch {} };
const bindings = new Map();
const pendingComputations = new Map();
let computationId = 0;
const namespaces = new Set(['common', 'shell']);
let manifest, catalog, locale = 'en', request = 0;
let preference = stored('dc2-locale') || localeCookie(document.cookie);
let recent;
try { recent = JSON.parse(stored('dc2-recent-locales', '[]')); } catch {}
if (!Array.isArray(recent)) recent = [];
let menu, trigger;
const t = (id, params) => catalog.message(locale, id, params);
const fmt = (kind, options, value) => new Intl[kind](intlLocales(locale, manifest), options).format(value);
function clearStaticBinding(element, attribute) {
  if (!attribute) element.removeAttribute('data-i18n');
  else if (element.dataset.i18nAttrs) {
    const attributes = JSON.parse(element.dataset.i18nAttrs);
    delete attributes[attribute];
    if (Object.keys(attributes).length) element.dataset.i18nAttrs = JSON.stringify(attributes);
    else element.removeAttribute('data-i18n-attrs');
  }
}
export const i18n = {
  get locale() { return locale; }, get manifest() { return manifest; }, t,
  number: (value, options = {}) => value == null ? '—' : fmt('NumberFormat', options, value),
  percent: (value, options = {}) => value == null ? '—' : fmt('NumberFormat', { style: 'percent', maximumFractionDigits: 1, ...options }, value),
  date: (value, options = {}) => value == null ? '—' : fmt('DateTimeFormat', options, new Date(value)),
  relative: (value, unit) => new Intl.RelativeTimeFormat(intlLocales(locale, manifest), { numeric: 'auto' }).format(value, unit),
  compare: (a, b) => new Intl.Collator(intlLocales(locale, manifest)).compare(a, b),
  // Explicit product label adapters only; never pass user-authored data here.
  label(value, namespace = 'common') {
    const key = catalog.sourceKey(namespace, value, 'label_');
    return key ? t(`${namespace}.${key}`) : value;
  },
  activity(value) {
    const known = new Set('requirements specification repository_analysis research diagnosis architecture_design work_planning coding configuration refactoring dependency_or_build_change test_authoring documentation_authoring data_or_schema_change build_validation unit_testing integration_testing browser_qa compatibility_testing migration_rehearsal verification_review packaging deployment rollback runtime_operations monitoring user_elaboration status_update completion_handoff review_feedback coordination accounting_overhead mixed unknown unattributed'.split(' '));
    return known.has(value) ? t('common.activity_' + value) : value;
  },
  error(id, params = {}) {
    const error = new Error();
    Object.defineProperty(error, 'message', { get: () => t(id, params) });
    return error;
  },
  // Only explicitly marked product text is translated; never scan arbitrary user text.
  markup(id, params = {}) { return `<span data-i18n="${escape(id)}" data-i18n-args="${escape(JSON.stringify(params))}">${escape(t(id, params))}</span>`; },
  formatted(kind, value, options = {}) {
    return `<span data-i18n-format="${escape(JSON.stringify({ kind, value, options }))}">${escape(i18n[kind](value, options))}</span>`;
  },
  computedMarkup(compute) {
    const id = String(++computationId);
    pendingComputations.set(id, compute);
    return `<span data-i18n-compute="${id}">${escape(compute())}</span>`;
  },
  computedAttribute(attribute, compute) {
    const id = String(++computationId);
    pendingComputations.set(id, compute);
    return `data-i18n-compute-attr="${id}" data-i18n-compute-attribute="${escape(attribute)}"`;
  },
  text(element, id, params = {}, attribute = null) {
    if (!element) return;
    clearStaticBinding(element, attribute);
    const value = t(id, params);
    const entries = bindings.get(element) || new Map();
    entries.set(attribute || 'textContent', { id, params, attribute, last: String(value ?? '') }); bindings.set(element, entries);
    if (attribute) element.setAttribute(attribute, value); else element.textContent = value;
  },
  bind(element, compute, attribute = null) {
    if (!element) return;
    clearStaticBinding(element, attribute);
    const value = compute();
    const entries = bindings.get(element) || new Map();
    entries.set(attribute || 'textContent', { compute, attribute, last: String(value ?? '') }); bindings.set(element, entries);
    if (attribute) element.setAttribute(attribute, value); else element.textContent = value;
  },
  async ensure(...names) { names.flat().forEach(name => namespaces.add(name)); return catalog.ensure(locale, names.flat()); },
  async setLocale(value) {
    const nextPreference = value === null ? null : matchLocale(value, manifest.locales);
    if (value !== null && !nextPreference) throw new Error('Unsupported language');
    const next = negotiateLocale(nextPreference, navigator.languages, manifest);
    const ticket = ++request;
    const failures = [], loaded = new Set();
    // Navigation can introduce a namespace while a language file is in flight.
    // Finish that namespace too before committing the selected language.
    while ([...namespaces].some(name => !loaded.has(name))) {
      const batch = [...namespaces].filter(name => !loaded.has(name));
      failures.push(...await catalog.ensure(next, batch));
      if (ticket !== request) return;
      batch.forEach(name => loaded.add(name));
    }
    locale = next; preference = nextPreference; save('dc2-locale', preference);
    if (preference) { recent = [...new Set([preference, ...recent])].slice(0, 5); save('dc2-recent-locales', JSON.stringify(recent)); }
    document.cookie = `dc2-locale=${preference ? encodeURIComponent(preference) : ''}; Path=/; SameSite=Lax; Max-Age=${preference ? 31536000 : 0}${location.protocol === 'https:' ? '; Secure' : ''}`;
    document.documentElement.lang = locale;
    document.documentElement.dir = manifest.locales.find(entry => entry.tag === locale)?.direction || 'ltr';
    refresh(document);
    document.dispatchEvent(new CustomEvent('dc2:localechange', { detail: { locale } }));
    if (trigger) trigger.querySelector('[data-language-tag]').textContent = shortLocale(locale);
    paintMenu();
    const status = document.getElementById('language-status');
    if (status) status.textContent = failures.length ? t('shell.languageFallback') : '';
  },
};
function refresh(root) {
  for (const element of [...(root.matches?.('[data-i18n-compute]') ? [root] : []), ...root.querySelectorAll('[data-i18n-compute]')]) {
    const id = element.dataset.i18nCompute;
    const compute = pendingComputations.get(id);
    if (compute) { i18n.bind(element, compute); pendingComputations.delete(id); element.removeAttribute('data-i18n-compute'); }
  }
  for (const element of [...(root.matches?.('[data-i18n-compute-attr]') ? [root] : []), ...root.querySelectorAll('[data-i18n-compute-attr]')]) {
    const id = element.dataset.i18nComputeAttr; const compute = pendingComputations.get(id);
    if (compute) { i18n.bind(element, compute, element.dataset.i18nComputeAttribute); pendingComputations.delete(id); element.removeAttribute('data-i18n-compute-attr'); element.removeAttribute('data-i18n-compute-attribute'); }
  }
  const selector = '[data-i18n], [data-i18n-attrs]';
  for (const element of [...(root.matches?.(selector) ? [root] : []), ...root.querySelectorAll(selector)]) {
    let params = {}; try { params = JSON.parse(element.dataset.i18nArgs || '{}'); } catch {}
    if (element.dataset.i18n) {
      const value = t(element.dataset.i18n, params);
      if (element.textContent !== value) element.textContent = value;
    }
    if (element.dataset.i18nAttrs) for (const [attribute, id] of Object.entries(JSON.parse(element.dataset.i18nAttrs))) {
      const value = t(id, params);
      if (element.getAttribute(attribute) !== value) element.setAttribute(attribute, value);
    }
  }
  for (const element of [...(root.matches?.('[data-i18n-format]') ? [root] : []), ...root.querySelectorAll('[data-i18n-format]')]) {
    const { kind, value, options } = JSON.parse(element.dataset.i18nFormat);
    if (!['number', 'percent', 'date'].includes(kind)) continue;
    const text = i18n[kind](value, options);
    if (element.textContent !== text) element.textContent = text;
  }
  for (const [element, entries] of bindings) {
    if (!element.isConnected) { bindings.delete(element); continue; }
    for (const [property, binding] of entries) {
      const { id, params, attribute, compute } = binding;
      const current = attribute ? element.getAttribute(attribute) : element.textContent;
      // A later success/error update may have intentionally replaced this text.
      // Never resurrect an obsolete message when switching languages.
      if (current !== binding.last) { entries.delete(property); continue; }
      const value = compute ? compute() : t(id, params);
      if (attribute) { if (element.getAttribute(attribute) !== String(value)) element.setAttribute(attribute, value); }
      else if (element.textContent !== String(value ?? '')) element.textContent = value;
      binding.last = String(value ?? '');
    }
  }
}
function displayName(tag, type = 'language') {
  if (type === 'language' && tag.startsWith('cnr-')) return t(tag.endsWith('Latn') ? 'shell.montenegrinLatin' : 'shell.montenegrinCyrillic');
  try {
    const name = new Intl.DisplayNames(intlLocales(locale, manifest), { type }).of(tag);
    if (name && name !== tag) return name;
  } catch {}
  return manifest.locales.find(entry => entry.tag === tag)?.englishName || tag;
}
function shortLocale(tag) {
  try { return new Intl.Locale(tag).language.toUpperCase(); } catch { return String(tag).slice(0, 2).toUpperCase(); }
}
function paintMenu() {
  if (!menu) return;
  const search = menu.querySelector('input');
  const query = search.value.trim().toLocaleLowerCase(locale);
  const browser = browserLocales(navigator.languages, manifest.locales);
  const rows = manifest.locales.filter(entry => entry.status === 'enabled');
  const byTag = new Map(rows.map(entry => [entry.tag, entry]));
  const matches = entry => [displayName(entry.tag), entry.nativeName, entry.tag].some(text => text.toLocaleLowerCase(locale).includes(query));
  const row = entry => `<button type="button" class="language-option" data-locale="${escape(entry.tag)}" aria-pressed="${locale === entry.tag}"><span class="language-flags">${entry.countries.map(code => `<img src="/icons/flags/${code.toLowerCase()}.svg" width="20" height="15" alt="${escape(displayName(code, 'region'))}" title="${escape(displayName(code, 'region'))}">`).join('')}</span><span class="language-names"><span>${escape(displayName(entry.tag))}</span><span lang="${escape(entry.tag)}" dir="${entry.direction}">${escape(entry.nativeName)}</span>${browser.includes(entry.tag) ? `<small>${escape(t('shell.browserPreference'))}</small>` : ''}</span><span class="language-check" aria-hidden="true">${locale === entry.tag ? '<span class="ti ti-circle-check"></span>' : ''}</span></button>`;
  const section = (label, entries) => entries.length ? `<section><h2>${escape(t(label))}</h2>${entries.map(row).join('')}</section>` : '';
  const currentFocus = document.activeElement?.dataset.locale;
  menu.querySelector('.language-results').innerHTML = section('shell.recent', recentLocales(recent, navigator.languages, manifest.locales).map(tag => byTag.get(tag)).filter(matches)) + section('shell.allLanguages', rows.filter(matches).sort((a, b) => i18n.compare(displayName(a.tag), displayName(b.tag))));
  if (!menu.querySelector('.language-option')) menu.querySelector('.language-results').textContent = t('shell.noLanguages');
  search.placeholder = t('shell.searchLanguages'); search.setAttribute('aria-label', t('shell.searchLanguages'));
  menu.setAttribute('aria-label', t('shell.language'));
  trigger.setAttribute('aria-label', `${t('shell.language')}: ${displayName(locale)}`);
  menu.querySelector('[data-browser-language]').textContent = t('shell.useBrowserLanguage');
  if (currentFocus) menu.querySelector(`[data-locale="${CSS.escape(currentFocus)}"]`)?.focus({ preventScroll: true });
}
function setupMenu() {
  const host = document.createElement('div'); host.className = 'language-picker';
  host.innerHTML = `<button type="button" class="btn btn-small" id="language-toggle" aria-expanded="false" aria-controls="language-menu"><span class="ti ti-world" aria-hidden="true"></span><span data-language-tag>${escape(shortLocale(locale))}</span></button><div id="language-menu" class="language-menu" hidden data-ui-contextual-overlay="Language selection"><input type="search"><div class="language-results"></div><button type="button" class="btn" data-browser-language></button><p id="language-status" role="status"></p></div>`;
  const home = document.querySelector('.who');
  home.prepend(host);
  menu = host.querySelector('#language-menu'); trigger = host.querySelector('#language-toggle');
  if (typeof menu.showPopover === 'function') menu.setAttribute('popover', 'manual');
  function place() {
    if (menu.hidden || !menu.hasAttribute('popover')) return;
    const anchor = trigger.getBoundingClientRect();
    const width = Math.min(340, innerWidth - 24);
    menu.style.width = `${width}px`;
    // Keep the selector aligned to the shell's right edge. The source direction
    // uses a stable right-aligned panel so long locale names never push it off
    // the header or move it with the account label.
    menu.style.left = `${Math.max(12, innerWidth - width - 12)}px`;
    menu.style.top = `${Math.min(anchor.bottom + 8, innerHeight - 80)}px`;
    menu.style.maxHeight = `${Math.max(60, innerHeight - Math.min(anchor.bottom + 8, innerHeight - 80) - 12)}px`;
  }
  function close(focus = false) { if (menu.hasAttribute('popover') && menu.matches(':popover-open')) menu.hidePopover(); menu.hidden = true; trigger.setAttribute('aria-expanded', 'false'); if (focus) trigger.focus(); }
  trigger.addEventListener('click', () => { if (!menu.hidden) return close(true); menu.hidden = false; trigger.setAttribute('aria-expanded', 'true'); menu.querySelector('input').value = ''; paintMenu(); if (menu.hasAttribute('popover')) menu.showPopover(); place(); menu.querySelector('input').focus(); });
  window.addEventListener('resize', place);
  window.addEventListener('scroll', place, { capture: true, passive: true });
  menu.querySelector('input').addEventListener('input', paintMenu);
  menu.addEventListener('click', async event => {
    const button = event.target.closest('button'); if (!button) return;
    const value = button.hasAttribute('data-browser-language') ? null : button.dataset.locale;
    if (value === undefined) return;
    button.disabled = true;
    try { await i18n.setLocale(value); close(true); }
    catch { document.getElementById('language-status').textContent = t('shell.languageFailed'); }
    finally { button.disabled = false; }
  });
  host.addEventListener('keydown', event => {
    if (menu.hidden) return;
    if (event.key === 'Escape') { event.preventDefault(); close(true); return; }
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    if (event.target.matches('input') && ['Home','End'].includes(event.key)) return;
    event.preventDefault();
    const buttons = [...menu.querySelectorAll('button:not([disabled])')];
    const index = buttons.indexOf(document.activeElement);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (index + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length;
    buttons[next]?.focus();
  });
  document.addEventListener('pointerdown', event => { if (!host.contains(event.target)) close(); });
  host.addEventListener('focusout', event => { if (!host.contains(event.relatedTarget)) close(); });
  // Native modal dialogs make the outer header inert. Reuse the same selector
  // inside the active dialog so changing language never requires losing a draft.
  new MutationObserver(() => {
    const dialog = [...document.querySelectorAll('dialog[open]')].findLast(element => element.matches(':modal'));
    const target = dialog ? dialog.querySelector('.dialog-head') || dialog : home;
    if (host.parentElement !== target) { close(); target.prepend(host); host.toggleAttribute('data-in-dialog', !!dialog); }
  }).observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ['open'] });
  paintMenu();
  window.addEventListener('languagechange', () => {
    if (preference === null) i18n.setLocale(null).catch(() => {});
    else paintMenu();
  });
}
const routes = { plan: ['plan'], progress: ['progress'], performance: ['performance'], usage: ['usage'], deployments: ['deployments'], tests: ['tests', 'evidence', 'artifacts'], sketches: ['sketches', 'evidence'], glossary: ['glossary'], health: ['health'], bugs: ['bugs'], admin: ['admin'] };
i18n.route = async () => i18n.ensure(...(routes[location.hash.slice(2).split(/[/?]/)[0]] || ['plan']));
i18n.ready = (async () => {
  manifest = await read('manifest.json'); catalog = createCatalog(manifest, read);
  await i18n.setLocale(matchLocale(preference, manifest.locales));
  await i18n.route();
  setupMenu();
  // Only explicit bindings created by product templates are observed. Changes to
  // user content or form values neither trigger translation nor replace nodes.
  new MutationObserver(records => {
    for (const element of bindings.keys()) if (!element.isConnected) bindings.delete(element);
    const roots = new Set(records.flatMap(record => [...record.addedNodes]).filter(node => node.nodeType === 1));
    for (const root of roots) {
      const marked = [...(root.matches('[data-i18n], [data-i18n-attrs]') ? [root] : []), ...root.querySelectorAll('[data-i18n], [data-i18n-attrs]')];
      const required = marked.flatMap(element => [element.dataset.i18n, ...Object.values(JSON.parse(element.dataset.i18nAttrs || '{}'))]).filter(Boolean).map(id => id.split('.')[0]);
      if (required.length) i18n.ensure(required).then(() => { if (root.isConnected) refresh(root); }).catch(() => {});
      else if (root.matches('[data-i18n-compute]') || root.querySelector('[data-i18n-compute]')) refresh(root);
    }
  }).observe(document.body, { childList: true, subtree: true });
})();
window.DevCoordinatorI18n = i18n;
