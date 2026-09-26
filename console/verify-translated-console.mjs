import fs from 'node:fs/promises';
import path from 'node:path';
import { formatMessage } from './i18n-core.mjs';

export async function verifyTranslatedConsole({ page, check, baseUrl, output, theme, viewport }) {
  const locale = process.env.CONSOLE_VERIFY_LOCALE || 'uk';
  const verify = (name, pass, detail = '') => check(`Translated ${locale} ${theme} ${viewport.width}: ${name}`, pass, detail);
  const manifest = JSON.parse(await fs.readFile(new URL('./locales/manifest.json', import.meta.url)));
  // A draft may be verified against its actual files before public admission.
  const entry = manifest.locales.find(entry => entry.tag === locale);
  if (!entry) throw new Error('Unknown test locale');
  entry.status = 'enabled';
  await page.route('**/locales/manifest.json', route => route.fulfill({ json: manifest }));
  const catalogs = {};
  for (const [ns, files] of Object.entries(entry.files)) {
    catalogs[ns] = Object.assign({}, ...await Promise.all(files.map(file => fs.readFile(new URL('./locales/' + file, import.meta.url), 'utf8').then(JSON.parse))));
  }
  await page.addInitScript(tag => localStorage.setItem('dc2-locale', tag), locale);
  const errors = []; page.on('pageerror', error => errors.push(error.message));
  const repository = 'r0123456789abcdef';
  const routes = ['deployments',`deployments?repository=${repository}`,'deployments/d0123456789abcdef',`plan/${repository}`,`progress/${repository}`,`usage/${repository}`,`performance/${repository}`,`decisions/${repository}`,`tests?repository=${repository}`,'health','health/containers','bugs','admin',`sketches/${repository}`];
  for (const route of routes) {
    await page.goto(`${baseUrl}#/${route}`);
    await page.waitForFunction(() => !document.querySelector('.skeleton'));
    await page.locator('main h1, main .notice').first().waitFor();
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    verify(`${route}: locale is active`, await page.locator('html').getAttribute('lang') === locale);
    const marked = await page.locator('[data-i18n]').evaluateAll(nodes => nodes.filter(node => node.getBoundingClientRect().width).map(node => ({ key: node.dataset.i18n, args: JSON.parse(node.dataset.i18nArgs || '{}'), value: node.textContent })));
    const wrong = [];
    for (const item of marked) {
      const dot = item.key.indexOf('.'); const pattern = catalogs[item.key.slice(0,dot)]?.[item.key.slice(dot+1)];
      if (pattern == null || item.value !== formatMessage(pattern,item.args,locale)) wrong.push(item.key);
    }
    verify(`${route}: rendered bindings use the real translated catalog`, wrong.length === 0, wrong.join(', '));
    if (route.startsWith('deployments') && route !== 'deployments') {
      const lifecycle = await page.locator('[data-cmd="deployment.start"], [data-cmd="deployment.stop"], [data-cmd="deployment.restart"]').evaluateAll(nodes => nodes.map(node => ({ action: node.dataset.cmd.split('.')[1], text: node.textContent })));
      verify(`${route}: lifecycle controls are localized`, lifecycle.length > 0 && lifecycle.every(item => item.text === catalogs.deployments['action_' + item.action]), JSON.stringify(lifecycle));
    }
    if (route.startsWith('usage/')) {
      const visible = await page.locator('main').innerText();
      if (locale !== 'en') verify('Usage coverage does not inject English environment nouns', !/\bconfigured Codex environment(s)?\b/i.test(visible), visible.match(/.{0,24}configured Codex environment.{0,24}/i)?.[0] || '');
    }
    if (route === 'bugs') {
      const fields = await page.locator('#bug-form label').evaluateAll(nodes => nodes.map(node => ({ field: node.querySelector('input,textarea').name, text: node.firstElementChild.textContent })));
      const keys = { component: 'component_ce54f0', summary: 'summary_8e76a9', expected: 'expected', actual: 'actual', steps: 'steps_1de3df' };
      verify('Bug report field labels are localized', fields.length === 5 && fields.every(item => item.text === catalogs.bugs[keys[item.field]]), JSON.stringify({formCount:await page.locator('#bug-form').count(), fields, expected: Object.fromEntries(Object.entries(keys).map(([field,key]) => [field,catalogs.bugs[key]]))}));
    }
    if (route === 'health') {
      const details = page.locator('.hi-repo-details').first();
      await details.locator('summary').click();
      const labels = await page.locator('.hi-repo-row [data-label]').evaluateAll(nodes => nodes.slice(0,3).map(node => node.dataset.label));
      verify('Health responsive metric names are localized', JSON.stringify(labels) === JSON.stringify([catalogs.health.cpu_db9a4c,catalogs.health.memory_c3963a,catalogs.health.storage_a69c4d]));
      const metrics = await details.locator('.hi-checkout small').first().innerText();
      verify('Health expanded checkout metrics are localized', metrics.includes(catalogs.health.memory_c3963a) && metrics.includes(catalogs.health.storage_a69c4d), metrics);
      await page.screenshot({path:path.join(output,`translated-health-${locale}-${theme}-${viewport.width}.png`),mask:[page.locator('#who-email')]});
    }
    if (route.startsWith('plan/')) {
      const tabs = await page.locator('#workspace-work-views a').evaluateAll(nodes => nodes.map(node => ({ view: node.getAttribute('href').split('/')[1], text: node.textContent })));
      verify('Plan secondary navigation is localized', tabs.length > 0 && tabs.every(tab => tab.text === catalogs.shell['view_' + tab.view]));
      const requested = page.locator('[data-elaborate-task][aria-disabled="true"] .plan-elaborate-label');
      const requestedLabels = await requested.allTextContents();
      verify('Plan explanation request status is localized', requestedLabels.length > 0 && requestedLabels.every(text => text === catalogs.plan.requested_2d9e28), JSON.stringify({actual:requestedLabels, expected:catalogs.plan.requested_2d9e28}));
      if (locale !== 'en') {
        const summaries = await page.locator('.plan-total-progress, .plan-task-meta, .plan-release-meta').allTextContents();
        verify('Plan generated summaries contain no English sentence fragments', summaries.every(text => !/\b(not estimated|lines done|tasks done|your request)\b/i.test(text)));
      }
    }
    const overflow = await page.evaluate(() => ({ width: document.documentElement.scrollWidth - innerWidth, elements: [...document.querySelectorAll('body *')].map(element => ({ tag: element.tagName, cls: element.className, right: element.getBoundingClientRect().right, left: element.getBoundingClientRect().left })).filter(item => item.right > innerWidth + 1 || item.left < -1).slice(0, 8) }));
    verify(`${route}: no document overflow`, overflow.width <= 1, JSON.stringify(overflow));
  }
  await page.goto(`${baseUrl}#/plan/${repository}`);
  await page.waitForFunction(() => !document.querySelector('.skeleton'));
  await page.locator('#language-toggle').click();
  const row = page.locator(`.language-option[data-locale="${locale}"]`).first();
  verify('native name retained', await row.locator('.language-names > span').nth(1).innerText() === entry.nativeName);
  verify('localized name and native name both present', await row.locator('.language-names > span').count() === 2);
  await page.screenshot({ path: path.join(output, `translated-${locale}-${theme}-${viewport.width}.png`), mask: [page.locator('#who-email')] });
  verify('no page errors across routes', errors.length === 0, errors.join('; '));
}
