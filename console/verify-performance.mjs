import path from 'node:path';

const REPO = 'r0123456789abcdef';
const metric = measured => ({ measured, exact: measured, unknown: 0 });
const effort = tokens => ({ operations: 4, retryOperations: 1, reworkOperations: 0, providerTotalTokens: metric(tokens), activeAgentMs: metric(42000), elapsedExecutionMs: metric(60000), recordedWaitMs: metric(18000), activities: { coding: metric(tokens * .6), integration_testing: metric(tokens * .4) } });
const coverage = { state: 'complete', has_gaps: false, configured_collectors: 1, available_collectors: 1, contributing_collectors: 1, unavailable_reasons: {}, database_schemas: [8], taxonomy_versions: [1] };
export function performanceFixture(operation, params, scenario = {}) {
  const end = params.window_end_ms || Date.UTC(2026, 8, 22); const start = params.window_start_ms ?? end - 86400000;
  const repo = params.repository_id || REPO;
  const makeReview = (id, revision, disposition = 'retained') => ({ reference: `${id}@${revision}`, record_id: id, revision, completed: disposition !== 'proposed', recorded_at_ms: end - revision * 1000,
    record: { version: 1, repositoryId: repo, projectId: repo, workstreamId: null, windowStartMs: end - 86400000, windowEndMs: end - 3600000, outcomeId: 'p1111111111111101', experiment: {
      hypothesis: 'Repeated usage reads delayed the same measured workload.', alternatives: ['Read in one batch.', 'Keep separate queries.'], chosenAction: disposition === 'unchanged' ? 'Keep the existing verification workflow.' : 'Read repository usage in one batch.', disposition,
      reason: 'The same workload used fewer tokens and passed the same quality checks.', baseline: { evidenceRefs: [], interpretation: 'The baseline measures the same workload.', missingMeasurements: [] }, evidenceRefs: [], resultEvidenceRefs: [], observations: [{ kind: 'intentional_validation', interpretation: 'Required validation remains part of both measurements.', evidenceRefs: [] }], comparison: { narrative: 'The same workload completed with fewer tokens.', inputChangeReason: null }, successCriteria: 'Reduce tokens with passing quality checks.', rollbackCondition: 'Revert if quality checks fail.', scopeRepoId: repo, preservesQuality: true,
    } } });
  const usage = (total, next) => {
    const base = total / 6400000;
    const rows = [
      ['p1111111111111101', 'Faster usage queries', 'improvement', 2400000], ['p1111111111111102', 'Reliable route recovery', 'improvement', 1600000],
      ['p1111111111111103', 'Console chart improvements', 'user_feedback', 1600000], ['p1111111111111104', 'Verification coverage', 'goal', 640000],
    ].map(([outcomeId, title, kind, tokens]) => ({ outcomeId, title, kind, workstreamId: null, effort: effort(tokens * base) }));
    const totals = effort(total); totals.activities = { coding: metric(3744000 * base), integration_testing: metric(2496000 * base), unknown: metric(160000 * base) };
    return { coverage: scenario.usageUnavailable ? { ...coverage, state: 'unavailable', has_gaps: true, available_collectors: 0, unavailable_reasons: { mapping_unavailable: 1, query_budget_exhausted: 1 } } : coverage,
      totals: { total_tokens: scenario.usageUnavailable ? null : total }, activities: [], time: {}, tools: {}, semantics: {},
      outcomes: { schemaVersion: 1, coverage: scenario.usageUnavailable ? 'unavailable' : 'complete', totals: scenario.usageUnavailable ? { ...effort(0), providerTotalTokens: { measured: 0, exact: null, unknown: 1 } } : totals, attributed: effort(6240000 * base), unattributed: effort(160000 * base), unattributedReasons: { outcome_not_declared: 1 }, rows: scenario.empty || scenario.usageUnavailable ? [] : next ? rows.slice(2) : rows.slice(0, 2), totalRows: scenario.empty || scenario.usageUnavailable ? 0 : 4, nextCursor: next || scenario.empty || scenario.usageUnavailable ? null : 'fixture:2', kinds: { improvement: metric(4000000 * base), user_feedback: metric(1600000 * base), goal: metric(640000 * base), unattributed: metric(160000 * base) }, basis: 'Provider total observations are counted once. Missing attribution remains explicit.' } };
  };
  if (operation === 'performance.overview') { const total = params.totals_only ? 32000000 : 6400000; const u = usage(total, !!params.outcome_cursor); return { repository_id: repo, window_start_ms: start, window_end_ms: end, generated_at_ms: end, total_tokens: u.outcomes.totals.providerTotalTokens, coverage: u.coverage, usage: params.totals_only ? null : u }; }
  if (operation === 'performance.reviews') return { total_reviews: scenario.empty ? 12 * 0 : 12, next_before: params.before || scenario.empty ? null : 20, records: scenario.empty ? [] : params.record_id ? [{ review: makeReview(params.record_id, params.before ? 1 : 2), revision_count: 2 }] : params.before ? [{ review: makeReview('review-older', 1, 'inconclusive'), revision_count: 1 }] : [{ review: makeReview('review-new', 2), revision_count: 2 }, { review: makeReview('review-unchanged', 1, 'unchanged'), revision_count: 1 }] };
  const [id, revision] = params.reference.split('@'); const r = makeReview(id, Number(revision), id.includes('older') ? 'inconclusive' : 'retained');
  return { review: r, usage: usage(1000000, !!params.outcome_cursor), evidence: [{ source: { kind: 'decision', reference: 'QUALITY' }, title: 'Keep verification quality', body: 'Preserve the quality checks while reducing repeated reads.', available: true }, { source: { kind: 'outcome', reference: 'p1111111111111101' }, title: 'Faster usage queries', body: '', available: true }, { source: { kind: 'run', reference: 'w1/expired-run' }, title: 'Run evidence', body: '', available: false }], comparisons: id.includes('older') ? [] : [{ source: { kind: 'usage', reference: 'fixture' }, window_start_ms: end - 3600000, window_end_ms: end, verified_improvement: true, explanation: 'Comparable workload and passing quality evidence.', metrics: [{ metric: 'tokens', before: metric(1000000), after: metric(750000), reduction: 250000, reduction_percent: 25 }, { metric: 'active_agent_ms', before: metric(42000), after: metric(28000), reduction: 14000, reduction_percent: 100 / 3 }] }] };
}

export async function verifyPerformance({ page, daemon, check, scenario, baseUrl, output, theme, viewport }) {
  const verify = (label, pass, detail = '') => check(`Performance ${theme} ${viewport.width}: ${label}`, pass, detail);
  const errors = []; page.on('pageerror', e => errors.push(e.message));
  daemon.setScenario(scenario);
  await page.emulateMedia({colorScheme:theme});
  await page.goto(`${baseUrl}#/performance/${REPO}`);
  await page.locator('[data-performance-history] tbody tr').first().waitFor();
  await page.waitForFunction(() => document.querySelector('[data-performance-lifetime]')?.textContent.includes('32'));
  verify('performance is a repository work view', await page.locator('#workspace-work-views a[aria-current]').innerText() === 'Performance');
  verify('overall and period totals are distinct', (await page.locator('[data-performance-lifetime]').innerText()).includes('32') && (await page.locator('[data-performance-period-total]').innerText()).includes('6.4'));
  verify('both grouping charts and outcome composition are visible', await page.locator('.performance-distribution').count() === 2 && await page.locator('.performance-outcome-table .performance-stack').count() === 3);
  await page.locator('.performance-exact summary').first().click();
  verify('exact values can be read without color', (await page.locator('.performance-exact[open]').innerText()).includes('3,744,000'));
  await page.locator('.performance-exact summary').first().click();
  await page.locator('[data-performance-outcomes-more] button').click();
  await page.locator('.performance-outcome-table tbody tr').filter({ hasText: 'Verification coverage' }).waitFor();
  verify('outcome pagination keeps full totals', (await page.locator('.performance-outcome-table tfoot').innerText()).includes('6.4') && await page.locator('.performance-outcome-table tbody tr').count() === 5);
  await page.locator('[data-performance-select]').first().click();
  await page.locator('[data-performance-reader] .performance-gain').first().waitFor();
  verify('selecting review changes only scoped usage', (await page.locator('[data-performance-scope-total]').innerText()).includes('1.0M') && (await page.locator('[data-performance-period-total]').innerText()).includes('6.4') && (await page.locator('[data-performance-lifetime]').innerText()).includes('32'));
  verify('comparison and rationale are present', (await page.locator('[data-performance-reader]').innerText()).includes('25.0%') && (await page.locator('[data-performance-reader]').innerText()).includes('Alternatives considered'));
  verify('review selection moves focus to its reader', await page.locator('[data-performance-reader]').evaluate(e => document.activeElement === e));
  await page.locator('[data-performance-revisions] summary').click();
  await page.locator('[data-performance-revision-more] button').click();
  await page.locator('[data-performance-revision-list] a').filter({ hasText: 'Revision 1' }).click();
  await page.waitForFunction(() => document.querySelector('[data-performance-reader]')?.textContent.includes('Revision 1'));
  verify('older immutable revision can be opened', page.url().includes('review-new%401'));
  await page.reload(); await page.locator('[data-performance-reader] .performance-gain').first().waitFor();
  verify('reload preserves exact review and overview dates', page.url().includes('review-new%401') && page.url().includes('from='));
  await page.locator('[data-performance-overview]').click();
  await page.waitForFunction(() => document.querySelector('[data-performance-scope-title]')?.textContent === 'Overview usage');
  await page.locator('[data-performance-history-more] button').click();
  await page.locator('[data-performance-select="review-older@1"]').click();
  await page.waitForFunction(() => document.querySelector('[data-performance-reader]')?.textContent.includes('No verified before-and-after'));
  verify('inconclusive review has no fabricated gain', await page.locator('[data-performance-reader] .performance-gain').count() === 0);
  await page.locator('[data-performance-range="30d"]').click();
  await page.waitForFunction(() => document.querySelector('[data-performance-range="30d"]')?.getAttribute('aria-pressed') === 'true');
  verify('range change returns to overview', !page.url().includes('review='));
  await page.locator('[data-performance-custom-open]').click();
  await page.waitForFunction(() => document.activeElement?.matches('.performance-date-dialog input[name=from]'));
  verify('custom dates receive focus in the viewport', await page.locator('.performance-date-dialog input[name=from]').evaluate(e => document.activeElement===e && e.getBoundingClientRect().left >= 0));
  await page.keyboard.press('Escape');
  verify('cancel dates restores the trigger', await page.locator('[data-performance-custom-open]').evaluate(e=>document.activeElement===e) && !await page.locator('.performance-date-dialog').evaluate(e=>e.open));
  await page.locator('[data-performance-custom-open]').click();
  await page.locator('.performance-date-dialog input[name=from]').fill('2026-09-10');
  await page.locator('.performance-date-dialog input[name=to]').fill('2026-09-12');
  const customResponse = page.waitForResponse(response => response.url().endsWith('/performance.overview') && response.request().postDataJSON().window_start_ms === Date.UTC(2026, 8, 10));
  await page.locator('.performance-date-dialog button[type=submit]').click();
  await customResponse;
  await page.waitForFunction(() => location.hash.includes('range=custom'));
  verify('custom range uses UTC boundaries', daemon.calls.some(call => call.operation === 'performance.overview' && call.params.window_start_ms === Date.UTC(2026, 8, 10) && call.params.window_end_ms === Date.UTC(2026, 8, 13)));
  await page.locator('[data-performance-select]').first().click(); await page.locator('[data-performance-reader] .performance-gain').first().waitFor();
  await page.screenshot({ path: path.join(output, `performance-${theme}-${viewport.width}.png`), fullPage: true });
  if (viewport.width <= 600) verify('outcome names retain a readable column', (await page.locator('.performance-outcome-table tbody tr td:first-child').first().boundingBox()).width >= 160);
  verify('no document horizontal overflow', await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1));
  verify('page has no JavaScript errors', errors.length === 0, errors.join('; '));
  await page.locator('[data-performance-reader] summary').filter({ hasText: 'Evidence and measurement' }).click();
  verify('expired evidence has no active link', await page.locator('.performance-evidence li').filter({ hasText: 'Evidence unavailable' }).locator('a').count() === 0);
  await page.locator('.performance-evidence a').filter({ hasText: 'Faster usage queries' }).click(); await page.locator('.plan-context').waitFor();
  verify('outcome link preserves exact task', page.url().includes('task=p1111111111111101'));
  await page.goto(baseUrl+'#/performance/r2');
  await page.locator('[data-performance-history] tbody tr').first().waitFor();
  verify('repository switch scopes the reads', daemon.calls.some(call => call.operation==='performance.reviews' && call.params.repository_id==='r2'));
  let failed=false;
  await page.route('**/api/v2/performance.overview',route=>{const params=route.request().postDataJSON();if(!params.totals_only && !failed){failed=true;return route.fulfill({status:503,contentType:'application/json',body:JSON.stringify({ok:false,error:{code:'unavailable',message:'Fixture measurements unavailable'}})});}return route.continue();});
  await page.locator('[data-performance-refresh]').click();
  await page.locator('[data-performance-charts] [role=alert]').waitFor();
  verify('measurement failure preserves review browsing', await page.locator('[data-performance-history] tbody tr').count()===2 && (await page.locator('[data-performance-outcomes]').innerText()).includes('could not be loaded'));
  await page.locator('[data-performance-charts] button').click();
  await page.locator('.performance-distribution').first().waitFor();
  verify('failed measurements can recover', await page.locator('.performance-distribution').count()===2);
  await page.unroute('**/api/v2/performance.overview');
  daemon.setScenario({...scenario,usageUnavailable:true});
  await page.locator('[data-performance-refresh]').click();
  await page.waitForFunction(()=>document.querySelector('[data-performance-charts]')?.textContent.includes('Token measurements are unavailable'));
  verify('query failures are not mislabeled as missing setup', (await page.locator('[data-performance-coverage]').innerText()).includes('Usage data unavailable'));
  verify('unavailable measurements do not become zero totals', await page.locator('[data-performance-period-total]').innerText()==='—' && await page.locator('[data-performance-history] tbody tr').count()===2);
  daemon.setScenario({...scenario,empty:true});
  await page.locator('[data-performance-refresh]').click();
  await page.waitForFunction(()=>document.querySelector('[data-performance-history]')?.textContent.includes('No performance reviews'));
  verify('empty review history is explicit', await page.locator('[data-performance-count]').innerText()==='0');
  daemon.setScenario({...scenario,admin:false,denied:true});
  await page.reload(); await page.locator('main .notice').waitFor();
  verify('review information stays administrator-only', (await page.locator('main .notice').innerText()).includes('Administrator access'));

}
