import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chooseRepository } from './verify-tests-pane.mjs';

// Leave the old document and settle its fixture work before clearing call evidence.
export async function isolateScenario(page, daemon, scenario) {
  await page.goto('about:blank');
  await daemon.drain();
  daemon.setScenario(scenario);
}

export function currentDocumentRequest(request, page, url, operations = null) {
  const requestUrl = new URL(request.url());
  const operation = requestUrl.pathname.match(/^\/api\/v3\/([a-z.]+)$/)?.[1];
  return requestUrl.origin === new URL(url).origin && request.frame() === page.mainFrame() && request.frame().url() === url && request.method() === 'POST'
    && Boolean(operation) && (operations ? operations.includes(operation) : operation !== 'user.whoami');
}

export function relevantCalls(calls) {
  const keys = ['repository_id', 'headers_only', 'after_repository_id', 'after_sequence', 'limit'];
  return calls.filter(call => ['user.whoami', 'plan.overview', 'repository.list', 'test.list', 'decision.tail', 'test.evidence.get', 'test.evidence.lookup'].includes(call.operation))
    .slice(-16).map(call => ({ operation: call.operation, params: Object.fromEntries(keys.filter(key => key in call.params).map(key => {
      const value = call.params[key];
      return [key, typeof value === 'string' ? value.slice(0, 80) : value === null || ['boolean', 'number'].includes(typeof value) ? value : '[omitted]'];
    })) }));
}

export function planReadEvidence(calls) {
  const headerPages = calls.filter(call => call.operation === 'plan.overview' && !call.params.repository_id && call.params.headers_only).length;
  const aggregateReads = calls.filter(call => call.operation === 'plan.overview' && !call.params.repository_id && !call.params.headers_only).length;
  const testsReads = calls.filter(call => call.operation === 'test.list').length;
  return { headerPages, aggregateReads, testsReads, valid: headerPages === 1 && aggregateReads === 0 && testsReads === 0, calls: relevantCalls(calls) };
}

export async function documentEvidence(page) {
  return page.evaluate(() => ({ pathname: location.pathname, route: location.hash.split('?')[0], readyState: document.readyState,
    appReady: typeof window.render === 'function', workspaceReady: Boolean(window.DevCoordinatorWorkspace),
    headingLinks: document.querySelectorAll('main h1 a.destination-link').length, skeletons: document.querySelectorAll('main .skeleton').length }));
}

export async function verifyWorkspace({ page, daemon, check, scenario, baseUrl, output, theme, viewport }) {
  const repositoryId = 'r0123456789abcdef';
  const verify = (name, condition, detail = '') => check(`workspace ${theme} ${viewport.width}: ${name}`, condition, detail);
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.addInitScript(() => {
    window.workspaceTiming = { lcp: 0, longTasks: [] };
    const ready = new MutationObserver(() => {
      if (document.querySelectorAll('.plan-task-select').length !== 354) return;
      ready.disconnect();
      window.workspaceTiming.domReadyAt = performance.now();
      requestAnimationFrame(() => requestAnimationFrame(() => { window.workspaceTiming.paintReadyAt = performance.now(); }));
    });
    ready.observe(document, { childList: true, subtree: true });
    new PerformanceObserver((list) => { for (const entry of list.getEntries()) window.workspaceTiming.lcp = entry.startTime; }).observe({ type: 'largest-contentful-paint', buffered: true });
    new PerformanceObserver((list) => { for (const entry of list.getEntries()) window.workspaceTiming.longTasks.push({ start: entry.startTime, duration: entry.duration }); }).observe({ type: 'longtask', buffered: true });
  });
  for (const [target, viewer] of [['rffffffffffffffff', false], ['rffffffffffffffff', true], ['release-clone', false]]) {
    await isolateScenario(page, daemon, { ...scenario, admin: !viewer, workspacePaging: true, workspaceViewer: viewer });
    await page.goto(`${baseUrl}#/plan/${target}`);
    await page.locator('.plan-context').waitFor();
    verify(`${viewer ? 'viewer' : 'administrator'} direct links beyond the first header page retain exact repository scope`, page.url().endsWith(target) && await page.locator('#workspace-heading').innerText() === (target === 'release-clone' ? 'repo-one' : 'Late repository') && daemon.calls.some((call) => call.operation === 'plan.overview' && call.params.repository_id === target) && await page.locator('.plan-task-select').count() === 0);
    if (viewer) verify('viewer direct links use the authorized Plan point read', daemon.calls.some((call) => call.operation === 'plan.overview' && call.params.repository_id === target && call.params.limit === 1));
    verify('direct route content does not request global Plan aggregates', !daemon.calls.some((call) => call.operation === 'plan.overview' && !call.params.repository_id && !call.params.headers_only) && await page.locator('.plan-context').isVisible());
    if (target === 'release-clone') verify('direct clone is grouped with its canonical source from first render', await page.locator('#repository-list .workspace-repository-name').allTextContents().then((names) => names.filter((name) => name === 'repo-one').length === 1 && !names.includes('0.1.3-generated')));
    daemon.releaseDelayed();
  }
  if ([1440, 390].includes(viewport.width)) {
    await isolateScenario(page, daemon, { ...scenario, workspaceLargePlan: true, workspaceDeepPlan: true, workspacePaging: true, workspaceHeaderPending: true });
    await page.goto(`${baseUrl}#/plan/${repositoryId}`);
    await page.waitForFunction(() => document.querySelectorAll('.plan-task-select').length === 354);
    await page.evaluate(() => document.fonts.ready);
    await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    const timing = await page.evaluate(() => ({ ...window.workspaceTiming, readyAt: performance.now(), rows: document.querySelectorAll('.plan-task-select').length }));
    verify('complete 354-task Plan renders all four scoped pages while catalogue continuation is held',
      [0, 100, 200, 300].every((after) => daemon.calls.some((call) => call.operation === 'plan.overview' && call.params.repository_id === repositoryId && (call.params.after_sequence || 0) === after && !call.params.limit))
      && !daemon.calls.some((call) => call.operation === 'plan.overview' && !call.params.repository_id && !call.params.headers_only), JSON.stringify(timing));
    verify('selected Plan traversal starts before metadata and is consumed once after metadata settles', daemon.calls.findIndex(call => call.operation === 'plan.overview' && call.params.repository_id === repositoryId && !call.params.limit) < daemon.calls.findIndex(call => call.operation === 'plan.overview' && call.params.headers_only) && daemon.calls.filter(call => call.operation === 'plan.overview' && call.params.repository_id === repositoryId && !call.params.limit).length === 4 && await page.locator('.grow.grel').count() === 26 && await page.locator('.plan-preview-notice').isVisible());
    await page.screenshot({ path: path.join(output, `workspace-large-current-${theme}-${viewport.width}.png`), mask: [page.locator('#who-email')] });
    await page.goto('about:blank');
    daemon.releaseDelayed();
  }
  await isolateScenario(page, daemon, { ...scenario, workspaceLongList: true, workspaceDeepPlan: true, workspaceStableOptional: true, planPaged: true });
  await page.goto(`${baseUrl}#/plan/${repositoryId}`);
  await page.waitForSelector('.plan-context');
  const initialHeading = await page.locator('#workspace-heading > span').elementHandle();
  const initialAspect = await page.locator('#workspace-aspects a[aria-current]').elementHandle();
  daemon.releaseDelayed();
  await daemon.waitForCall('usage.repositories');
  await daemon.waitForCall('progress.repositories');
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  verify('timeline controls stay visible at both horizontal scroll limits', await page.evaluate(async () => {
    const viewport = document.querySelector('[data-plan-viewport]');
    const original = viewport.scrollLeft;
    const visible = () => [...document.querySelectorAll('.plan-toolbar button')].every(button => {
      const box = button.getBoundingClientRect();
      const frame = viewport.getBoundingClientRect();
      return box.left >= frame.left - 1 && box.right <= frame.right + 1 && box.width > 0;
    });
    let result = true;
    for (const left of [0, viewport.scrollWidth]) {
      viewport.scrollLeft = left;
      await new Promise(resolve => requestAnimationFrame(resolve));
      result &&= visible();
    }
    viewport.scrollLeft = original;
    return result;
  }));
  verify('unchanged optional totals retain navigation content and current aspect nodes', await initialHeading.evaluate(node => node.isConnected) && await initialAspect.evaluate(node => node.isConnected));
  const initialPlanRead = call => call.operation === 'plan.overview' && call.params.repository_id === repositoryId && !call.params.limit && !call.params.after_sequence && !call.params.after_release_sequence;
  const planReads = daemon.calls.filter(initialPlanRead).length;
  const refreshCallStart = daemon.calls.length;
  const previousPlan = await page.locator('.plan-context').elementHandle();
  const refreshUrl = page.url();
  const [refreshRequest] = await Promise.all([
    page.waitForRequest(request => currentDocumentRequest(request, page, refreshUrl, ['plan.overview']) && request.postDataJSON()?.repository_id === repositoryId),
    page.evaluate(() => window.render()),
  ]);
  verify('refresh reads the current Plan again instead of reusing the consumed model', daemon.calls.filter(initialPlanRead).length === planReads + 1 && !await previousPlan.evaluate(node => node.isConnected));
  verify('direct plan links select the same repository', await page.locator('#workspace-heading').innerText() === 'repo-one');
  verify('one shared selector replaces page-specific pickers', await page.locator('#repository-list').count() === 1 && await page.locator('[data-project-picker], .test-repository-list').count() === 0);
  const expectedAspects = [
    ['Plan & progress', `#/plan/${repositoryId}`],
    ['Deployments', `#/deployments?repository=${repositoryId}`],
    ['Tests', `#/tests?repository=${repositoryId}`],
    ['Sketches', `#/sketches/${repositoryId}`],
    ['Decisions', `#/decisions/${repositoryId}`],
    ['Glossary', `#/glossary/${repositoryId}`],
  ];
  verify('all aspects have real repository-scoped links', await page.locator('#workspace-aspects a').evaluateAll((links) => links.map((link) => [link.textContent, link.getAttribute('href')])).then((links) => JSON.stringify(links) === JSON.stringify(expectedAspects)));
  verify('plan and progress are one destination with four views', await page.locator('#workspace-work-views a').allTextContents().then((labels) => labels.join() === 'Plan,Progress,Usage,Performance'));
  const reads = planReadEvidence(daemon.calls);
  verify('Plan reads one metadata header page without aggregate or Tests barriers', reads.valid,
    reads.valid ? '' : JSON.stringify({ ...reads, triggerRequest: { operation: 'plan.overview', method: refreshRequest.method(), currentDocument: refreshRequest.frame() === page.mainFrame() && refreshRequest.frame().url() === refreshUrl }, document: await documentEvidence(page) }));
  const pages = daemon.calls.slice(refreshCallStart).filter(call => call.operation === 'plan.overview' && call.params.repository_id === repositoryId && !call.params.limit);
  verify('Plan loads every page of the same revision', pages.length > 1 && initialPlanRead(pages[0]) && pages.slice(1).every(call => (call.params.after_sequence > 0 || call.params.after_release_sequence > 0) && /^[a-f0-9]{64}$/.test(call.params.expected_revision) && call.params.expected_revision === pages[1].params.expected_revision));
  verify('tasks on later pages remain available', await page.locator('[data-task-row="p1111111111111106"]').count() === 1);
  await page.locator('.plan-task-select[data-select-task="p1111111111111106"]').click();
  verify('a task from a later page can be selected', await page.locator('[data-task-row="p1111111111111106"]').evaluate(row => row.classList.contains('selected')));
  verify('expanded selected-task text stays within its column', await page.locator('.plan-selection').evaluate(tray => {
    if (tray.classList.contains('collapsed')) return true;
    const heading = tray.querySelector('.plan-selection-heading');
    const bounds = heading.getBoundingClientRect();
    return [...heading.children].filter(child => child.getBoundingClientRect().width > 0)
      .every(child => { const box = child.getBoundingClientRect(); return box.left >= bounds.left - 1 && box.right <= bounds.right + 1; });
  }));
  const planEndpoint = '**/api/v3/plan.overview';
  let staleOnce = false;
  await page.route(planEndpoint, (route) => {
    if ((route.request().postDataJSON().after_sequence || route.request().postDataJSON().after_release_sequence) && !staleOnce) {
      staleOnce = true;
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify({ ok: false, error: { code: 'cursor_stale', message: 'Fixture plan edited during loading' } }) });
    }
    return route.continue();
  });
  await page.reload();
  await page.locator('.plan-context').waitFor();
  verify('a changed plan restarts loading without mixing pages', staleOnce && await page.locator('[data-task-row="p1111111111111106"]').count() === 1);
  await page.unroute(planEndpoint);
  let failedPage = false;
  await page.route(planEndpoint, (route) => {
    if ((route.request().postDataJSON().after_sequence || route.request().postDataJSON().after_release_sequence)) {
      failedPage = true;
      return route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ ok: false, error: { code: 'unavailable', message: 'Fixture continuation unavailable' } }) });
    }
    return route.continue();
  });
  await page.reload();
  await page.getByText('Fixture continuation unavailable', { exact: false }).waitFor();
  verify('a failed later page never renders a partial plan', failedPage && await page.locator('.plan-context').count() === 0);
  await page.unroute(planEndpoint);
  await page.reload();
  await page.locator('.plan-context').waitFor();
  verify('reload recovers the complete plan after a page failure', await page.locator('[data-task-row="p1111111111111106"]').count() === 1);
  if (viewport.width > 760) {
    verify('sidebar starts wide enough for ordinary repository names', (await page.locator('#repository-sidebar').boundingBox()).width >= 280);
    const resize = page.locator('#repository-resize');
    await resize.focus(); await page.keyboard.press('ArrowRight');
    verify('keyboard resizes the sidebar', await resize.getAttribute('aria-valuenow') === '290');
    const handle = await resize.boundingBox();
    await page.mouse.move(handle.x + handle.width / 2, handle.y + 100); await page.mouse.down();
    await page.mouse.move(handle.x + handle.width / 2 + 40, handle.y + 100); await page.mouse.up();
    verify('drag resizes the sidebar', await resize.getAttribute('aria-valuenow') === '330');
    await page.reload(); await page.locator('.plan-context').waitFor();
    verify('saved width survives reload', await resize.getAttribute('aria-valuenow') === '330');
    await page.locator('#repository-close').click();
    verify('sidebar can be hidden without losing repository context', !await page.locator('#repository-sidebar').isVisible() && await page.locator('#workspace-heading').innerText() === 'repo-one');
    await page.reload(); await page.locator('.plan-context').waitFor();
    verify('hidden preference survives reload', !await page.locator('#repository-sidebar').isVisible());
    await page.locator('#repository-toggle').click();
    await resize.focus(); await page.keyboard.press('Home');
    for (let step = 0; step < 4; step++) await page.keyboard.press('ArrowRight');
    verify('repository names remain on one readable line', await page.locator('.workspace-repository[aria-current] > span').evaluate((element) => getComputedStyle(element).whiteSpace === 'nowrap'));
  }
  await page.locator('#repository-presentation').click();
  const appearance = page.locator('#repository-presentation-dialog');
  verify('appearance editor opens immediately with focused name input', await appearance.locator('input[name=display_name]').evaluate((element) => document.activeElement === element));
  await appearance.locator('input[name=display_name]').fill('Discarded name');
  await appearance.locator('[data-presentation-cancel]').click();
  verify('Cancel preserves the saved repository name and restores focus', await page.locator('#workspace-heading').innerText() === 'repo-one' && await page.locator('#repository-presentation:focus').count() === 1);
  await page.locator('#repository-presentation').click();
  await appearance.locator('input[name=display_name]').fill('   ');
  await appearance.locator('button[type=submit]').click();
  verify('blank names cannot be saved', await appearance.isVisible() && !daemon.calls.some((call) => call.operation === 'repository.presentation.update'));
  await appearance.locator('input[name=display_name]').fill('My project');
  await appearance.locator('input[value=rocket]').check();
  await page.route('**/api/v3/repository.presentation.update', (route) => route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ ok: false, error: { code: 'unavailable', message: 'Fixture save unavailable' } }) }));
  await appearance.locator('button[type=submit]').click();
  await appearance.locator('[role=alert]:not([hidden])').waitFor();
  verify('failed appearance save keeps entered name and icon', await appearance.locator('input[name=display_name]').inputValue() === 'My project' && await appearance.locator('input[value=rocket]').isChecked());
  await page.unroute('**/api/v3/repository.presentation.update');
  await appearance.locator('button[type=submit]').click();
  await appearance.waitFor({ state: 'detached' });
  if (viewport.width <= 760) await page.locator('#repository-toggle').click();
  await page.locator('#workspace-checkouts > summary').click();
  await daemon.waitForCall((call) => call.operation === 'repository.list' && call.params.after_worktree_id === 'wfeature');
  await page.waitForFunction(() => document.querySelector('#workspace-checkouts')?.textContent.includes('/repo-one/.worktrees/release-candidate'));
  verify('background checkout pages retain the just-saved name and icon', await page.locator('#workspace-heading').innerText() === 'My project' && await page.locator('.workspace-repository[aria-current]').getAttribute('data-repository-icon') === 'rocket');
  await page.locator('#workspace-checkouts > summary').click();
  if (viewport.width <= 760) await page.locator('#repository-close').click();
  await page.reload(); await page.locator('.plan-context').waitFor();
  verify('name and icon persist through reload without changing repository identity', await page.locator('#workspace-heading').innerText() === 'My project' && await page.locator('.workspace-repository[aria-current]').getAttribute('data-repository-icon') === 'rocket' && (await page.url()).endsWith(repositoryId));
  await page.locator('#repository-presentation').click();
  await page.screenshot({ path: path.join(output, `repository-appearance-${theme}-${viewport.width}.png`), mask: [page.locator('#who-email')] });
  await appearance.locator('[data-presentation-reset]').click();
  await appearance.locator('button[type=submit]').click();
  await appearance.waitFor({ state: 'detached' });
  verify('Use defaults restores the original name and folder icon', await page.locator('#workspace-heading').innerText() === 'repo-one' && await page.locator('.workspace-repository[aria-current]').getAttribute('data-repository-icon') === 'folder');
  await page.locator('.plan-task-meta > .badge:visible').first().waitFor();
  const statusCounts = { releases: await page.locator('.plan-release-meta > .badge:visible').count(), tasks: await page.locator('.plan-task-meta > .badge:visible').count() };
  verify('plan readability checks cover populated release and task statuses', statusCounts.releases > 0 && statusCounts.tasks > 0, JSON.stringify(statusCounts));
  const feedback = page.locator('[data-task-row="p1111111111111106"] .plan-task-select');
  verify('feedback fixture includes an estimate, work status and request status', await feedback.locator('.plan-task-meta > *').count() === 3 && /775.*1 not estimated/.test(await feedback.innerText()));
  const compressedTitles = await page.locator('.plan-task-title').evaluateAll((elements) => elements.filter((element) => {
    const lineHeight = parseFloat(getComputedStyle(element).lineHeight);
    return element.getBoundingClientRect().height + 1 < Math.min(element.scrollHeight, lineHeight * 2);
  }).length);
  verify('plan metadata does not compress task titles into partial lines', compressedTitles === 0);
  const croppedEstimates = await page.locator('.plan-task-meta > span:first-child > span').evaluateAll((elements) => elements.flatMap((element) => {
    if (!element.getClientRects().length) return [];
    const clip = element.parentElement.getBoundingClientRect();
    const range = document.createRange(); range.selectNodeContents(element);
    const cut = [...range.getClientRects()].find(rect => rect.top < clip.top - .5 || rect.bottom > clip.bottom + .5);
    return cut ? [{ clip: clip.toJSON(), fragment: cut.toJSON() }] : [];
  }));
  verify('estimate text fragments fit the metadata line without vertical cropping', croppedEstimates.length === 0, JSON.stringify(croppedEstimates));
  const clippedStatuses = await page.locator('.plan-release-meta > .badge, .plan-task-meta > .badge').evaluateAll((badges) => badges.flatMap((badge) => {
    if (!badge.getClientRects().length) return [];
    const row = badge.closest('.grow');
    const walker = document.createTreeWalker(badge, NodeFilter.SHOW_TEXT);
    const fragments = [badge.getBoundingClientRect()];
    while (walker.nextNode()) {
      if (!walker.currentNode.textContent.trim()) continue;
      const range = document.createRange(); range.selectNodeContents(walker.currentNode);
      fragments.push(...range.getClientRects());
    }
    for (let ancestor = badge; ancestor && row?.contains(ancestor); ancestor = ancestor.parentElement) {
      const style = getComputedStyle(ancestor), rect = ancestor.getBoundingClientRect();
      const clipsX = /hidden|clip/.test(style.overflowX), clipsY = /hidden|clip/.test(style.overflowY);
      const clipped = fragments.find((fragment) => clipsX && (fragment.left < rect.left - 1 || fragment.right > rect.right + 1)
        || clipsY && (fragment.top < rect.top - 1 || fragment.bottom > rect.bottom + 1));
      if (clipped) return [{ row: row.className, status: badge.innerText.trim(), clip: ancestor.className, fragment: clipped.toJSON(), bounds: rect.toJSON() }];
    }
    return [];
  }));
  verify('release and task status badges remain readable inside their timeline rows', clippedStatuses.length === 0, JSON.stringify(clippedStatuses));
  if (clippedStatuses.length) await page.screenshot({ path: path.join(output, `plan-status-clipping-${theme}-${viewport.width}.png`), fullPage: true });
  const timelineScroll = await page.locator('[data-plan-viewport]').evaluate(element => ({ top: element.scrollTop, left: element.scrollLeft }));
  await feedback.scrollIntoViewIfNeeded();
  await page.screenshot({ path: path.join(output, `plan-feedback-status-${theme}-${viewport.width}.png`), mask: [page.locator('#who-email')] });
  await page.locator('[data-plan-viewport]').evaluate((element, scroll) => { element.scrollTop = scroll.top; element.scrollLeft = scroll.left; }, timelineScroll);
  const deepParent = page.locator('[data-collapse="p1111111111111200"]');
  const deepChild = page.locator('[data-task-row="p1111111111111210"]');
  await deepParent.click();
  verify('deep task controls still collapse their descendants', await deepParent.getAttribute('aria-expanded') === 'false' && !await deepChild.isVisible());
  await deepParent.click();
  await deepChild.waitFor();
  const sidebar = await page.locator('#repository-sidebar').elementHandle();
  for (const [view, selector, operation] of [
    ['progress', '.progress-workspace', 'progress.repository'],
    ['usage', '[data-ui-region="usage-primary-trend"]', 'usage.repository'],
    ['plan', '.plan-context', 'plan.overview'],
  ]) {
    daemon.calls.length = 0;
    await page.locator(`#workspace-work-views a[href="#/${view}/${repositoryId}"]`).click();
    await page.locator(selector).waitFor();
    verify(`${view} keeps the selected repository and queries its records`, daemon.calls.some((call) => call.operation === operation && call.params.repository_id === repositoryId) && await page.locator('#workspace-heading').innerText() === 'repo-one');
    const overflow = await page.evaluate(() => ({ width: document.documentElement.scrollWidth, viewport: innerWidth, elements: [...document.querySelectorAll('main > *, .plan-selection > *, .plan-context > *')].map((element) => ({ selector: element.className, right: element.getBoundingClientRect().right })).filter((element) => element.right > innerWidth + 1) }));
    verify(`${view} fits the viewport`, overflow.width <= overflow.viewport, JSON.stringify(overflow));
    if (overflow.width > overflow.viewport) await page.screenshot({ path: path.join(output, `${view}-overflow-${viewport.width}-${theme}.png`) });
  }
  await page.locator('#workspace-aspects a').filter({ hasText: 'Deployments' }).click();
  await page.locator('.deployment-record').first().waitFor();
  verify('deployments are scoped, without duplicate cross-aspect summaries', await page.locator('.deployment-record').count() === 2 && await page.locator('.deployment-repository-summary').count() === 0);
  verify('domains stay beside their deployments', await page.locator('.deployment-domain').count() === 2 && await page.locator('[data-edit-domain]').count() === 2);
  verify('deployment cards fit the selected-repository pane', await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
  verify('switching aspects keeps the original sidebar element', await sidebar.evaluate((element) => element.isConnected));
  await page.locator('[data-edit-domain]').first().click();
  await page.locator('#domain-dialog[open]').waitFor();
  await page.keyboard.press('Escape');
  verify('canceling a domain edit preserves the repository', await page.locator('#workspace-heading').innerText() === 'repo-one');
  await page.locator('.deployment-record-title a').first().click();
  await page.locator('[data-cmd="deployment.restart"]').first().waitFor();
  verify('deployment detail remains inside the workspace', await page.locator('#workspace-heading').innerText() === 'repo-one');
  await page.goBack();
  await page.locator('.deployment-record').first().waitFor();
  await page.locator('#workspace-aspects a').filter({ hasText: 'Decisions' }).click();
  await page.locator('#decision-search').waitFor();
  verify('the summary does not displace decision search', !await page.locator('.story').getAttribute('open') && (await page.locator('#decision-search').boundingBox()).y < 500);
  await page.locator('.story summary').click();
  verify('the decision summary opens in context', await page.locator('.story[open]').count() === 1);
  await page.locator('.story summary').click();
  await page.locator('#decision-search input').fill('export');
  await page.locator('#decision-search button[type="submit"]').click();
  await page.waitForFunction(() => document.querySelector('main')?.textContent.includes('Exports are downloadable files'));
  verify('decisions search uses the current repository', daemon.calls.some((call) => call.operation === 'decision.search' && call.params.repository_id === repositoryId));
  await chooseRepository(page, 'a-very-long-deployment-name');
  await page.waitForFunction(() => location.hash === '#/decisions/r2');
  await page.locator('#decision-search').waitFor();
  verify('switching repository preserves the current aspect', (await page.locator('#workspace-heading').innerText()).startsWith('a-very-long'));
  await page.reload();
  await page.locator('#decision-search').waitFor();
  verify('reload retains repository and aspect', (await page.locator('#workspace-heading').innerText()).startsWith('a-very-long') && await page.locator('#workspace-aspects a[aria-current]').innerText() === 'Decisions');
  await page.goBack();
  await page.locator('#decision-search').waitFor();
  verify('Back restores the prior repository', await page.locator('#workspace-heading').innerText() === 'repo-one');
  await page.locator('#workspace-aspects a').filter({ hasText: 'Tests' }).click();
  await page.locator('.test-result').first().waitFor();
  if (viewport.width <= 760) {
    verify('narrow layout puts results before the repository drawer', await page.locator('.test-result').first().evaluate(element => {
      const sidebar = document.querySelector('#repository-sidebar');
      return !sidebar.getClientRects().length && element.getBoundingClientRect().y < 330;
    }));
    await page.click('#repository-toggle');
    verify('repository drawer focuses search', await page.locator('#repository-search:focus').count() === 1);
    verify('open phone drawer keeps background controls inactive', await page.locator('#repository-sidebar').getAttribute('role') === 'dialog' && await page.locator('.workspace-content').evaluate(element => element.inert && element.getAttribute('aria-hidden') === 'true'));
    await page.keyboard.press('Escape');
    verify('Escape returns to the repository trigger', await page.locator('#repository-toggle:focus').count() === 1 && !await page.locator('#repository-sidebar').isVisible());
    verify('closing the drawer restores background interaction', await page.locator('.workspace-content').evaluate(element => !element.inert && !element.hasAttribute('aria-hidden')) && await page.locator('#repository-sidebar').getAttribute('role') === null);
    await page.click('#repository-toggle');
  }
  const overlappingLabels = await page.locator('#repository-list a').evaluateAll((elements) => elements.filter((element) => {
    const row = element.getBoundingClientRect();
    const text = element.querySelector('span').getBoundingClientRect();
    return text.top < row.top - 1 || text.bottom > row.bottom + 1;
  }).length);
  verify('long duplicate checkout paths stay inside their own repository rows', overlappingLabels === 0);
  await page.locator('#repository-search').fill('no-such-repository');
  verify('search has a truthful empty state', await page.locator('#repository-list a').count() === 0 && /No matching/.test(await page.locator('#repository-list').innerText()));
  await page.locator('#repository-search').fill('repo-one');
  verify('search filters the shared list', await page.locator('#repository-list a').count() === 1);
  await page.locator('#repository-search').fill('');
  if (viewport.width <= 760) await page.click('#repository-close');
  await page.click('#nav-toggle');
  await page.locator('#nav a[data-view="health"]').click();
  await page.locator('.hi-host').waitFor();
  verify('host tools do not pretend to be repository-scoped', !await page.locator('#workspace-context').isVisible());
  await page.click('#nav-toggle');
  await page.locator('#nav a[data-view="plan"]').click();
  await page.locator('.plan-context').waitFor();
  verify('returning from host tools remembers the repository', await page.locator('#workspace-heading').innerText() === 'repo-one');
  await page.locator(`#workspace-aspects a[href="#/sketches/${repositoryId}"]`).click();
  await page.locator('.mockup-gallery [data-collection] .notice').waitFor();
  verify('Sketches navigation reads the selected repository archive', page.url().endsWith(`#/sketches/${repositoryId}`)
    && await page.locator('#workspace-heading').innerText() === 'repo-one'
    && await page.locator('#workspace-aspects a[aria-current]').innerText() === 'Sketches'
    && daemon.calls.some(call => call.operation === 'design.sketch.list' && call.params.repository_id === repositoryId && call.params.limit === 24 && !call.params.offset));
  verify('an unpublished sketch archive has a truthful empty state', await page.locator('.mockup-gallery [data-collection]').innerText().then(text => text.includes('No sketches have been published for this project yet.'))
    && await page.locator('.mockup-card').count() === 0);
  await page.goto(`${baseUrl}#/glossary/${repositoryId}`);
  const glossaryAspect = page.locator(`#workspace-aspects a[aria-current][href="#/glossary/${repositoryId}"]`);
  await glossaryAspect.waitFor({ state: 'visible' });
  await page.evaluate(() => new Promise(requestAnimationFrame));
  const {selectedAspect,aspectStrip}=await glossaryAspect.evaluate(element=>({selectedAspect:element.getBoundingClientRect().toJSON(),aspectStrip:element.parentElement.getBoundingClientRect().toJSON()}));
  const aspectVisible = !!(selectedAspect && aspectStrip && selectedAspect.width > 0 && selectedAspect.height > 0
    && selectedAspect.x >= aspectStrip.x - 1 && selectedAspect.x + selectedAspect.width <= aspectStrip.x + aspectStrip.width + 1);
  if (!aspectVisible) await page.screenshot({ path: path.join(output, `workspace-aspect-failure-${theme}-${viewport.width}.png`), fullPage: true });
  verify('direct links reveal the entire selected aspect on narrow screens', aspectVisible, JSON.stringify({ selectedAspect, aspectStrip }));
  await page.goto(`${baseUrl}#/plan/${repositoryId}`);
  await page.locator('.plan-context').waitFor();
  await page.screenshot({ path: path.join(output, `workspace-${theme}-${viewport.width}.png`), fullPage: true });
  await page.route('**/api/v3/test.list', route => route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ ok: false, error: { code: 'internal_error', message: 'Fixture Tests unavailable' } }) }));
  await page.goto(`${baseUrl}#/tests?repository=release-clone`);
  await page.reload();
  await page.locator('#workspace-aspects a').first().waitFor();
  await page.goto(`${baseUrl}#/plan/release-clone`);
  await page.locator('.plan-context').waitFor();
  verify('source heading survives unavailable Tests and a direct clone link', await page.locator('#workspace-heading').innerText() === 'repo-one');
  verify('selected source root is visible', await page.locator('#workspace-root').isVisible() && await page.locator('#workspace-root').innerText() === 'Root: /srv/repos/repo-one');
  verify('clone keeps its own exact plan scope', page.url().endsWith('/plan/release-clone') && await page.locator('#workspace-record-scope').innerText() === '/srv/releases/0.1.3-generated');
  if (viewport.width <= 760) await page.click('#repository-toggle');
  verify('sidebar shows source root without generated clone names', await page.locator('#repository-list .workspace-repository-name').allTextContents().then(names => names.includes('repo-one') && !names.includes('0.1.3-generated')) && await page.locator('.workspace-repository[aria-current] small').innerText() === '/srv/repos/repo-one');
  await page.locator('#repository-search').fill('/srv/releases/0.1.3-generated');
  verify('search by an exact checkout root finds its source repository', await page.locator('#repository-list a').count() === 1 && await page.locator('#repository-list .workspace-repository-name').innerText() === 'repo-one');
  await page.locator('#repository-search').fill('');
  await page.screenshot({ path: path.join(output, `repository-roots-${theme}-${viewport.width}.png`), fullPage: true, mask: [page.locator('#who-email')] });
  await page.unroute('**/api/v3/test.list');
  await isolateScenario(page, daemon, { ...scenario, delayMs: 1, delayOperations: ['plan.overview'] });
  await page.goto(`${baseUrl}#/plan/${repositoryId}`);
  await daemon.waitForCall('repository.list');
  await page.goto(`${baseUrl}#/health`);
  await page.locator('.hi-host').waitFor();
  daemon.releaseDelayed();
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  verify('navigation away aborts pending Plan data without overwriting the new route', page.url().endsWith('#/health') && await page.locator('.hi-host').isVisible() && await page.locator('.plan-context').count() === 0 && !await page.locator('#workspace-context').isVisible());
  verify('no uncaught browser errors', errors.length === 0, errors.join('; '));
}

// Non-browser guards for this existing acceptance harness; no installed daemon is used.
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url) && process.argv.includes('--self-test')) {
  const { default: test } = await import('node:test');
  const { default: assert } = await import('node:assert/strict');
  await test('scenario reset follows document departure and completed fixture drain', async () => {
    const order = [], scenario = {};
    let finishDrain;
    const isolation = isolateScenario({ goto: async url => { order.push(url); } }, {
      drain: () => new Promise(resolve => { order.push('drain'); finishDrain = resolve; }),
      setScenario: value => { assert.equal(value, scenario); order.push('reset'); },
    }, scenario);
    await Promise.resolve();
    assert.deepEqual(order, ['about:blank', 'drain']);
    finishDrain(); await isolation;
    assert.deepEqual(order, ['about:blank', 'drain', 'reset']);
    await assert.rejects(isolateScenario({ goto: async () => {} }, { drain: async () => { throw new Error('not idle'); }, setScenario: () => assert.fail('reset after failed drain') }, scenario), /not idle/);
  });
  await test('loading admission accepts only the intended document, origin, method and operation', () => {
    const url = 'http://console.example.test/#/tests/fixture', frame = { url: () => url }, page = { mainFrame: () => frame };
    const request = { url: () => 'http://console.example.test/api/v3/test.evidence.lookup', frame: () => frame, method: () => 'POST' };
    const accepts = value => currentDocumentRequest(value, page, url, ['test.evidence.get', 'test.evidence.lookup']);
    assert.equal(accepts(request), true);
    for (const wrong of [{ frame: () => ({ url: () => url }) }, { method: () => 'GET' }, { url: () => 'http://other.example.test/api/v3/test.evidence.lookup' }, { url: () => 'http://console.example.test/api/v3/plan.overview' }]) assert.equal(accepts({ ...request, ...wrong }), false);
    assert.equal(currentDocumentRequest(request, page, url + '-old'), false);
    assert.equal(currentDocumentRequest({ ...request, url: () => 'http://console.example.test/api/v3/user.whoami' }, page, url), false);
  });
  await test('Plan guards catch missing/duplicate headers, aggregate reads and Tests barriers without rejecting scoped reads', () => {
    const header = { operation: 'plan.overview', params: { headers_only: true } };
    const scoped = { operation: 'plan.overview', params: { repository_id: 'fixture', after_sequence: 100 } };
    assert.equal(planReadEvidence([header, scoped, scoped]).valid, true);
    for (const calls of [[], [header, header], [header, { operation: 'plan.overview', params: {} }], [header, { operation: 'test.list', params: {} }]]) assert.equal(planReadEvidence(calls).valid, false);
    const safe = relevantCalls(Array.from({ length: 20 }, () => ({ ...header, client: { identity: 'private' }, params: { ...header.params, query: 'private', cookie: 'private' } })));
    assert.equal(safe.length, 16);
    assert.equal(JSON.stringify(safe).includes('private'), false);
    assert.equal(relevantCalls([{ ...header, params: { repository_id: 'x'.repeat(500), headers_only: { private: true } } }])[0].params.repository_id.length, 80);
    assert.equal(JSON.stringify(relevantCalls([{ ...header, params: { headers_only: { private: true } } }])).includes('private'), false);
  });
}
