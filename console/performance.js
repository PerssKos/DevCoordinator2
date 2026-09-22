'use strict';
window.DevCoordinatorPerformance = (() => {
  const DAY = 86400000;
  const labels = { user_feedback: 'User feedback', improvement: 'Improvement', goal: 'Goal', stub: 'Unfinished work', unknown: 'Unknown kind', unattributed: 'Unattributed' };
  const statuses = { proposed: 'Proposed', applied: 'Applied', retained: 'Retained', reverted: 'Reverted', unchanged: 'No change', inconclusive: 'Inconclusive' };
  const activityLabel = key => key.replaceAll('_', ' ').replace(/^./, value => value.toUpperCase());
  const day = value => new Date(value).toISOString().slice(0, 10);
  const date = value => new Date(value).toLocaleDateString('en-US', { timeZone: 'UTC', month: 'short', day: 'numeric', year: 'numeric' });
  const period = (start, end) => `${date(start)} – ${date(end - 1)} · UTC`;
  function create({ api, esc, compactNumber, durationMs, coverageText, identity }) {
    const sessions = new Map();
    const cache = new Map();
    const amount = (value, time = false) => {
      if (!value || value.exact == null && !value.measured) return '—';
      const format = time ? durationMs : compactNumber;
      return `${value.exact == null ? '≥ ' : ''}${format(value.measured)}`;
    };
    const exact = value => value ? `${Number(value.measured).toLocaleString('en-US')} measured tokens${value.exact == null ? '; incomplete measurement' : ''}` : 'Measurement unavailable';
    const badge = disposition => `<span class="performance-status ${esc(disposition)}">${esc(statuses[disposition] || disposition)}</span>`;
    const color = key => ({coding:'var(--chart-blue)',integration_testing:'var(--chart-violet)',unknown:'var(--muted)',unattributed:'var(--muted)'})[key] || `var(--chart-${['blue', 'violet', 'amber', 'teal', 'green'][Array.from(key).reduce((n, c) => n + c.charCodeAt(0), 0) % 5]})`;
    const entries = values => Object.entries(values || {}).sort((a, b) => b[1].measured - a[1].measured || a[0].localeCompare(b[0]));
    function mix(values, total) {
      if (!total) return '<span class="muted">No measured tokens</span>';
      return `<span class="performance-stack" role="img" aria-label="${esc(entries(values).map(([key, value]) => `${activityLabel(key)}: ${exact(value)}`).join('; '))}">${entries(values).map(([key, value]) => `<span style="width:${Math.max(0, Math.min(100, value.measured / total * 100))}%;background:${key === 'unattributed' || key === 'unknown' ? 'var(--muted)' : color(key)}" title="${esc(`${labels[key] || activityLabel(key)}: ${exact(value)}`)}"></span>`).join('')}</span>`;
    }
    function distribution(title, values, total, kind = false) {
      const rows = entries(values); const sum = rows.reduce((n, [, value]) => n + value.measured, 0);
      return `<section class="performance-distribution"><div class="performance-section-head"><h2>${esc(title)}</h2><span>Total <strong title="${esc(exact(total))}">${esc(amount(total))}</strong></span></div>${mix(values, sum)}<div class="performance-legend">${rows.slice(0, 5).map(([key, value]) => `<span><i style="background:${key === 'unattributed' ? 'var(--muted)' : color(key)}"></i>${esc(kind ? labels[key] || activityLabel(key) : activityLabel(key))}<strong>${esc(amount(value))}</strong></span>`).join('')}${rows.length > 5 ? `<span>${rows.length - 5} more activities</span>` : ''}</div><details class="performance-exact"><summary>Exact values</summary><table><thead><tr><th>${kind ? 'Plan item kind' : 'Activity'}</th><th>Measured tokens</th><th>Share</th></tr></thead><tbody>${rows.map(([key, value]) => `<tr><td>${esc(kind ? labels[key] || activityLabel(key) : activityLabel(key))}</td><td>${Number(value.measured).toLocaleString('en-US')}${value.exact == null ? ' (partial)' : ''}</td><td>${sum ? (value.measured / sum * 100).toFixed(1) + '%' : '—'}</td></tr>`).join('')}</tbody></table></details></section>`;
    }
    async function render(root, repositoryId, signal) {
      if (!identity()?.administrator) { root.innerHTML = '<h1><a class="destination-link" href="#/performance">Performance</a></h1><p class="notice">Administrator access is required to read performance reviews.</p>'; return; }
      if (!repositoryId) { root.innerHTML = '<h1><a class="destination-link" href="#/performance">Performance</a></h1><p>No repository is selected.</p>'; return; }
      let session = sessions.get(repositoryId);
      if (!session) { session = { end: Date.now(), lifetimeEnd: Date.now(), range: '7d', scroll: 0 }; if (sessions.size >= 16) sessions.delete(sessions.keys().next().value); sessions.set(repositoryId, session); }
      const query = new URLSearchParams(location.hash.split('?')[1] || '');
      const range = ['7d', '30d', '90d', 'custom'].includes(query.get('range')) ? query.get('range') : session.range;
      const parsedEnd = Number(query.get('to'));
      const end = query.has('to') && Number.isSafeInteger(parsedEnd) && parsedEnd > 0 ? Math.min(parsedEnd, Date.now()) : session.end;
      const parsedStart = Number(query.get('from'));
      const start = query.has('from') && Number.isSafeInteger(parsedStart) && parsedStart >= 0 && parsedStart < end ? parsedStart : Math.max(0, end - (parseInt(range, 10) || 7) * DAY);
      session.end = end; session.range = range;
      const reference = query.get('review');
      const active = () => !signal.aborted && root.querySelector('[data-performance-page]');
      const find = name => root.querySelector(`[data-performance-${name}]`);
      const requests = { repository_id: repositoryId, window_start_ms: start, window_end_ms: end, outcome_limit: 20 };
      let currentUsage = null; let reviewData = null; let historyBefore = session.history?.next_before ?? null; let revisionBefore = null;
      function route(review, overrides = {}) {
        const p = new URLSearchParams({ range, from: String(start), to: String(end), ...overrides });
        if (review) p.set('review', review);
        return `#/performance/${encodeURIComponent(repositoryId)}?${p}`;
      }
      function navigate(review, overrides) {
        if (!reference) session.scroll = window.scrollY;
        location.hash = route(review, overrides);
      }
      function errorPanel(target, message, retry) {
        target.innerHTML = `<p role="alert">${esc(message)}</p><button type="button" class="btn btn-small">Retry</button>`;
        target.querySelector('button').addEventListener('click', retry);
      }
      async function read(operation, params, useCache = true) {
        const key = `${operation}:${JSON.stringify(params)}`; const saved = cache.get(key);
        if (useCache && saved && Date.now() - saved.at < 60000) return saved.data;
        const data = await api(operation, params);
        if (active()) { if (cache.size > 32) cache.delete(cache.keys().next().value); cache.set(key, { at: Date.now(), data }); }
        return data;
      }
      const previousHeight = root.querySelector("[data-performance-page]") ? root.getBoundingClientRect().height : 0;
      if (previousHeight) root.style.minHeight = previousHeight + "px";
      root.innerHTML = `<section data-performance-page><header class="performance-heading"><h1><a class="destination-link" href="#/performance">Performance</a></h1><div class="performance-range" aria-label="Overview period">${['7d', '30d', '90d'].map(value => `<button type="button" class="btn btn-small" data-performance-range="${value}" aria-pressed="${range === value}">${value}</button>`).join('')}<button type="button" class="btn btn-small" data-performance-custom-open>Custom</button><dialog class="performance-date-dialog"><form><h2>Custom dates</h2><label>From (UTC)<input type="date" name="from" value="${day(start)}" required></label><label>Through (UTC)<input type="date" name="to" value="${day(end - 1)}" max="${day(Date.now())}" required></label><p role="alert"></p><button class="btn btn-primary" type="submit">Apply dates</button><button class="btn" type="button" data-performance-cancel-dates>Cancel</button></form></dialog></div><span class="performance-dates">${esc(period(start, end))}</span><button type="button" class="btn btn-small" data-performance-refresh>Refresh</button></header>
        <div class="performance-totals" aria-label="Repository totals"><div><span>All recorded tokens</span><strong data-performance-lifetime>Loading…</strong></div><div><span>Overview period tokens</span><strong data-performance-period-total>Loading…</strong></div><div><span>All reviews</span><strong data-performance-count>Loading…</strong></div></div>
        <div class="performance-scope"><div><h2 data-performance-scope-title>${reference ? 'Review usage' : 'Overview usage'}</h2><span data-performance-scope-total></span></div>${reference ? '<button type="button" class="btn btn-small" data-performance-overview>Return to overview</button>' : ''}</div><p class="performance-coverage muted" data-performance-coverage role="status">Loading measurements…</p>
        <div class="performance-distributions" data-performance-charts><p>Loading token breakdowns…</p></div>
        <div class="performance-workspace"><div class="performance-primary"><section class="performance-outcomes"><div class="performance-section-head"><h2>Tokens by outcome</h2><span data-performance-outcome-count></span></div><div data-performance-outcomes>Loading outcomes…</div><div data-performance-outcomes-more></div></section>
        <section class="performance-history"><h2>Review history</h2><div data-performance-history>Loading reviews…</div><div data-performance-history-more></div></section></div>
        <aside class="performance-reader" data-performance-reader tabindex="-1" aria-label="Selected performance review">${reference ? '<p>Loading review…</p>' : '<h2>Review details</h2><p class="muted">Select a review to see its decisions, measurements and evidence.</p>'}</aside></div></section>`;
      root.querySelectorAll('[data-performance-range]').forEach(button => button.addEventListener('click', () => { const nextEnd = Date.now(); navigate(null, { range: button.dataset.performanceRange, from: String(nextEnd - parseInt(button.dataset.performanceRange, 10) * DAY), to: String(nextEnd) }); }));
      find('overview')?.addEventListener('click', () => navigate(null));
      find('refresh').addEventListener('click', () => { cache.clear(); session.history = null; session.periodUsage = null; session.end = Date.now(); session.lifetimeEnd = Date.now(); window.render(); });
      const form = root.querySelector('.performance-date-dialog form');
      const custom = root.querySelector('.performance-date-dialog');
      root.append(custom);
      find('custom-open').addEventListener('click', () => custom.showModal());
      custom.addEventListener('close', () => find('custom-open')?.focus());
      find('cancel-dates').addEventListener('click', () => custom.close());
      form.addEventListener('submit', event => { event.preventDefault(); const from = Date.parse(form.elements.from.value + 'T00:00:00Z'); const to = Math.min(Date.parse(form.elements.to.value + 'T00:00:00Z') + DAY, Date.now()); if (!Number.isFinite(from) || !Number.isFinite(to) || from < 0 || from >= to) { form.querySelector('[role=alert]').textContent = 'Choose a start date on or before the end date.'; return; } navigate(null, { range: 'custom', from: String(from), to: String(to) }); });
      function placeReader() {
        const reader = find('reader'); if (!reader || !reference) return;
        const selectedRow = [...find('history').querySelectorAll('[data-performance-select]')].find(link => link.dataset.performanceSelect.split('@')[0] === reference.split('@')[0])?.closest('tr');
        if (matchMedia('(max-width: 1000px)').matches && selectedRow) {
          let host = root.querySelector('.performance-inline-reader');
          if (!host) { host = document.createElement('tr'); host.className = 'performance-inline-reader'; host.innerHTML = '<td colspan="3"></td>'; selectedRow.after(host); }
          host.firstElementChild.append(reader);
        } else { root.querySelector('.performance-workspace').append(reader); root.querySelector('.performance-inline-reader')?.remove(); }
      }
      window.addEventListener('resize', placeReader, { signal });
      const showCoverage = usage => { find('coverage').textContent = `${coverageText(usage.coverage)}${usage.outcomes.unattributed.operations ? ' · Some work has no recorded outcome.' : ''}`; };
      function showUsage(usage, append = false) {
        const previousUsage = currentUsage; currentUsage = usage;
        if (!reference) { session.periodUsage = { start, end, usage: append && previousUsage ? { ...usage, outcomes: { ...usage.outcomes, rows: [...previousUsage.outcomes.rows, ...usage.outcomes.rows] } } : usage }; currentUsage = session.periodUsage.usage; }
        const outcomes = usage.outcomes; const total = outcomes.totals.providerTotalTokens;
        if (!append) {
          showCoverage(usage);
          find('scope-total').textContent = `${reference ? 'Review' : 'Period'} total: ${amount(total)}`;
          find('charts').innerHTML = distribution('Tokens by activity', outcomes.totals.activities, total) + distribution('Tokens by Plan item kind', outcomes.kinds, total, true);
          find('outcomes').innerHTML = `<table class="performance-outcome-table"><thead><tr><th>Outcome</th><th>Plan item kind</th><th>Activity mix</th><th>Total tokens</th></tr></thead><tbody></tbody><tfoot><tr><th colspan="3">Total for ${reference ? 'review' : 'period'}</th><td>${esc(amount(total))}</td></tr></tfoot></table>`;
        }
        if (usage.coverage.state === 'unavailable' && !total.measured) find('charts').innerHTML = '<p class="muted">Token measurements are unavailable for this period.</p>';
        const body = find('outcomes').querySelector('tbody');
        const unassigned = { outcomeId: null, title: 'Unattributed', effort: outcomes.unattributed };
        const rows = [...outcomes.rows, ...(!append && (outcomes.unattributed.operations || outcomes.unattributed.providerTotalTokens.measured) ? [unassigned] : [])];
        body.insertAdjacentHTML('beforeend', rows.map(row => `<tr><td data-label="Outcome">${row.outcomeId && row.title ? `<a href="#/plan/${encodeURIComponent(repositoryId)}?task=${encodeURIComponent(row.outcomeId)}">${esc(row.title)}</a>` : esc(row.title || 'Outcome no longer available')}</td><td data-label="Plan item kind">${esc(row.outcomeId ? labels[row.kind] || 'Unknown kind' : '—')}</td><td data-label="Activity mix">${mix(row.effort.activities, row.effort.providerTotalTokens.measured)}</td><td data-label="Total tokens" title="${esc(exact(row.effort.providerTotalTokens))}">${esc(amount(row.effort.providerTotalTokens))}</td></tr>`).join(''));
        if (!rows.length && !append) body.innerHTML = '<tr><td colspan="4">No measured outcomes for this period.</td></tr>';
        find('outcome-count').textContent = `${outcomes.totalRows} attributed outcomes`;
        find('outcomes-more').innerHTML = outcomes.nextCursor ? '<button type="button" class="btn btn-small">Load more outcomes</button>' : '';
        find('outcomes-more').querySelector('button')?.addEventListener('click', async event => {
          event.target.disabled = true;
          try { const data = reference ? await read('performance.review', { repository_id: repositoryId, reference, outcome_cursor: currentUsage.outcomes.nextCursor, outcome_limit: 20 }, false) : await read('performance.overview', { ...requests, outcome_cursor: currentUsage.outcomes.nextCursor }, false); if (active()) showUsage(data.usage, true); }
          catch (error) { if (active()) errorPanel(find('outcomes-more'), error.message, () => { cache.clear(); window.render(); }); }
        });
      }
      function evidenceMarkup(item) {
        const ref = item.source; let href = null;
        if (item.available && ref.kind === 'outcome') href = `#/plan/${encodeURIComponent(repositoryId)}?task=${encodeURIComponent(ref.reference)}`;
        if (item.available && ref.kind === 'decision') href = `#/decisions/${encodeURIComponent(repositoryId)}?q=${encodeURIComponent(ref.reference)}`;
        if (item.available && ref.kind === 'run' && ref.reference.includes('/')) { const [worktree, run] = ref.reference.split('/'); href = `#/tests?repository=${encodeURIComponent(repositoryId)}&run=${encodeURIComponent(run)}&worktree=${encodeURIComponent(worktree)}`; }
        return `<li>${item.available && ref.kind === 'release' ? `<button class="btn btn-small" type="button" data-performance-release="${esc(ref.reference)}">View delivery evidence</button>` : ''}${href ? `<a href="${esc(href)}">${esc(item.title)}</a>` : `<strong>${esc(item.title)}</strong>`}${!item.available ? '<span class="muted"> · Evidence unavailable</span>' : ''}${item.body ? `<p>${esc(item.body)}</p>` : ''}</li>`;
      }
      function showReview(data) {
        reviewData = data; const r = data.review; const e = r.record.experiment;
        find('scope-title').textContent = `Review usage · ${period(r.record.windowStartMs, r.record.windowEndMs)}`;
        showUsage(data.usage);
        const text = (title, value) => value ? `<h3>${title}</h3><p>${esc(value)}</p>` : '';
        find('reader').innerHTML = `<div class="performance-section-head"><h2>${esc(date(r.recorded_at_ms))} · Review</h2>${badge(e.disposition)}</div><p class="muted">Revision ${r.revision}${r.completed ? '' : ' · Review in progress'}</p>${text('Decision', e.chosenAction)}
          <section class="performance-comparison"><h3>Performance comparison</h3>${data.comparisons.length ? data.comparisons.map(comparison => `<div>${comparison.verified_improvement ? '<strong class="performance-gain">Verified improvement</strong>' : '<strong>Comparison not verified as an improvement</strong>'}<p class="muted">${esc(period(comparison.window_start_ms, comparison.window_end_ms))}</p>${comparison.metrics.map(metric => { const time = metric.metric !== 'tokens'; const title = { tokens: 'Tokens', active_agent_ms: 'Active agent time', elapsed_execution_ms: 'Elapsed execution time', recorded_wait_ms: 'Recorded waiting time' }[metric.metric]; const max = Math.max(metric.before.measured, metric.after.measured, 1); return `<div class="performance-metric"><h4>${title}</h4><div class="performance-before-after">${[['Before', metric.before], ['After', metric.after]].map(([label, value]) => `<div><span>${label}</span><strong>${esc(amount(value, time))}</strong><meter min="0" max="${max}" value="${value.measured}" aria-label="${esc(`${title} ${label.toLowerCase()}: ${amount(value, time)}`)}"></meter></div>`).join('')}</div><p class="${metric.reduction > 0 && comparison.verified_improvement ? 'performance-gain' : 'muted'}">${metric.reduction == null ? 'Incomplete measurement' : `${metric.reduction > 0 ? 'Reduction' : metric.reduction < 0 ? 'Increase' : 'No change'}${metric.reduction ? ': ' + esc(time ? durationMs(Math.abs(metric.reduction)) : compactNumber(Math.abs(metric.reduction))) : ''}${metric.reduction_percent == null ? '' : ` (${Math.abs(metric.reduction_percent).toFixed(1)}%)`}`}</p></div>`; }).join('')}<p class="muted">${esc(comparison.explanation)}</p></div>`).join('') : '<p class="muted">No verified before-and-after measurements are recorded for this review.</p>'}</section>
          <details open><summary>Reasoning and alternatives</summary>${text('Hypothesis', e.hypothesis)}<h3>Alternatives considered</h3><ul>${e.alternatives.map(value => `<li>${esc(value)}</li>`).join('')}</ul>${text('Rationale', e.reason)}${text('Baseline', e.baseline.interpretation)}${text('Comparison', e.comparison?.narrative)}${text('Changed inputs', e.comparison?.inputChangeReason)}${text('Success criteria', e.successCriteria)}${text('Revert condition', e.rollbackCondition)}${e.observations.map(o => text(activityLabel(o.kind), o.interpretation)).join('')}</details>
          <details><summary>Evidence and measurement coverage</summary><ul class="performance-evidence">${data.evidence.map(evidenceMarkup).join('')}</ul>${e.baseline.missingMeasurements.length ? `<h3>Recorded measurement gaps</h3><ul>${e.baseline.missingMeasurements.map(gap => `<li>${esc(activityLabel(gap))}</li>`).join('')}</ul>` : ''}<p>${esc(data.usage.outcomes.basis)}</p></details>
          <details data-performance-revisions><summary>Revision history</summary><div data-performance-revision-list></div><div data-performance-revision-more></div></details>`;
        find('revisions').addEventListener('toggle', () => { if (find('revisions').open && !find('revision-list').children.length) loadRevisions(); });
        find('reader').querySelectorAll('[data-performance-release]').forEach(button => button.addEventListener('click', async () => {
          button.disabled = true;
          try { const receipt = await api('release.evidence', { reference: button.dataset.performanceRelease });
            if (active() && button.isConnected) { const info = document.createElement('p'); info.textContent = receipt.target + ' · ' + (receipt.qualified ? 'Verified delivery' : 'Verification pending') + (receipt.verified_at_ms ? ' · ' + date(receipt.verified_at_ms) : '');
              if (receipt.access && /^https?:\/\//.test(receipt.access)) { const link = document.createElement('a'); link.href = receipt.access; link.textContent = 'Open delivered result'; info.append(' · ', link); } button.after(info); button.remove(); }
          } catch (error) { if (active() && button.isConnected) { button.disabled = false; button.parentElement.querySelector('[role=alert]')?.remove(); const message = document.createElement('p'); message.setAttribute('role','alert'); message.textContent = error.message; button.after(message); } }
        }));
        placeReader();
        if (session.focusReview) { window.scrollTo({ top: session.activationScroll || 0 }); find('reader').focus({ preventScroll: true }); if (matchMedia('(max-width: 1000px)').matches) find('reader').querySelector('h2').scrollIntoView({ block: 'nearest' }); session.focusReview = false; }
      }
      async function loadRevisions(more = false) {
        try { const data = await read('performance.reviews', { repository_id: repositoryId, record_id: reviewData.review.record_id, ...(more ? { before: revisionBefore } : {}) }); if (!active()) return; revisionBefore = data.next_before;
          find('revision-list').insertAdjacentHTML('beforeend', data.records.map(({ review }) => `<p><a href="${esc(route(review.reference))}"${review.reference === reference ? ' aria-current="true"' : ''}>Revision ${review.revision} · ${esc(date(review.recorded_at_ms))}</a> ${badge(review.record.experiment.disposition)}</p>`).join(''));
          find('revision-more').innerHTML = revisionBefore ? '<button class="btn btn-small" type="button">Earlier revisions</button>' : ''; find('revision-more').querySelector('button')?.addEventListener('click', () => loadRevisions(true));
        } catch (error) { if (active()) errorPanel(find('revision-list'), error.message, () => loadRevisions()); }
      }
      async function loadHistory(more = false) {
        try { const data = !more && session.history ? session.history : await read('performance.reviews', { repository_id: repositoryId, ...(more ? { before: historyBefore } : {}) }); if (!active()) return; historyBefore = data.next_before; session.history = more && session.history ? { ...data, records: [...session.history.records, ...data.records] } : data; find('count').textContent = String(data.total_reviews);
          if (!more) find('history').innerHTML = data.records.length ? '<table><thead><tr><th>Date</th><th>Review decision</th><th>Status</th></tr></thead><tbody></tbody></table>' : '<p class="muted">No performance reviews have been recorded for this repository.</p>';
          find('history').querySelector('tbody')?.insertAdjacentHTML('beforeend', data.records.map(({ review, revision_count }) => `<tr${review.record_id === reference?.split('@')[0] ? ' class="selected"' : ''}><td>${esc(date(review.recorded_at_ms))}</td><td><a data-performance-select="${esc(review.reference)}" href="${esc(route(review.reference))}">${esc(review.record.experiment.chosenAction.length > 140 ? review.record.experiment.chosenAction.slice(0, 137) + "…" : review.record.experiment.chosenAction)}</a>${revision_count > 1 ? `<small>${revision_count} revisions</small>` : ''}</td><td>${badge(review.record.experiment.disposition)}</td></tr>`).join(''));
          placeReader();
          find('history-more').innerHTML = historyBefore ? '<button type="button" class="btn btn-small">Load earlier reviews</button>' : ''; find('history-more').querySelector('button')?.addEventListener('click', event => { event.target.disabled = true; loadHistory(true); });
        } catch (error) { if (active()) { if (!more) find('count').textContent = 'Unavailable'; errorPanel(more ? find('history-more') : find('history'), error.message, () => loadHistory(more)); } }
      }
      root.addEventListener('click', event => { if (event.target.closest('[data-performance-select]')) { session.focusReview = true; session.activationScroll = window.scrollY; if (!reference) session.scroll = window.scrollY; } }, { signal });
      async function loadPeriod() {
        try { const data = await read('performance.overview', requests); if (!active()) return; find('period-total').textContent = amount(data.total_tokens); if (!reference) { showUsage(session.periodUsage?.start === start && session.periodUsage?.end === end ? session.periodUsage.usage : data.usage); if (session.scroll) window.scrollTo({ top: session.scroll }); } }
        catch (error) { if (active()) { find('period-total').textContent = 'Unavailable'; if (!reference) { find('coverage').textContent = 'Measurements could not be loaded.'; find('outcomes').textContent = 'Measurements could not be loaded.'; errorPanel(find('charts'), error.message, loadPeriod); } } }
      }
      async function loadLifetime() {
        try { const data = await read('performance.overview', { repository_id: repositoryId, window_start_ms: 0, window_end_ms: session.lifetimeEnd, totals_only: true }); if (!active()) return; find('lifetime').textContent = amount(data.total_tokens); find('lifetime').title = coverageText(data.coverage); }
        catch (error) { if (active()) { find('lifetime').textContent = 'Unavailable'; find('lifetime').title = error.message; } }
      }
      async function loadReview() {
        try { const data = await read('performance.review', { repository_id: repositoryId, reference, outcome_limit: 20 }); if (active()) showReview(data); }
        catch (error) { if (active()) { errorPanel(find('reader'), error.message, loadReview); find('coverage').textContent = 'Review measurements are unavailable.'; find('charts').innerHTML = ''; find('outcomes').textContent = 'Review measurements are unavailable.'; } }
      }
      await Promise.allSettled([loadHistory(), loadPeriod(), loadLifetime(), ...(reference ? [loadReview()] : [])]);
      if (active()) root.style.minHeight = '';
      signal.addEventListener('abort', () => { root.style.minHeight = ''; }, { once: true });
    }
    return { render };
  }
  return { create };
})();
