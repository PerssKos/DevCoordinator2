'use strict';
window.DevCoordinatorPerformance = (() => {
  const DAY = 86400000;
  const labels = { user_feedback: 'User feedback', improvement: 'Improvement', goal: 'Goal', stub: 'Unfinished work', unknown: 'Unknown kind', unattributed: 'Unattributed' };
  const statuses = { proposed: 'Proposed', applied: 'Applied', retained: 'Retained', reverted: 'Reverted', unchanged: 'No change', inconclusive: 'Inconclusive' };
  const kindLabel = key => labels[key] ? window.DevCoordinatorI18n.t("performance.kind_" + key) : activityLabel(key);
  const activityLabel = key => window.DevCoordinatorI18n.activity(key);
  const day = value => new Date(value).toISOString().slice(0, 10);
  const date = value => new Date(value).toLocaleDateString(window.DevCoordinatorI18n.locale, { timeZone: 'UTC', month: 'short', day: 'numeric', year: 'numeric' });
  const period = (start, end) => `${date(start)} – ${date(end - 1)} · UTC`;
  function create({ api, esc, compactNumber, durationMs, coverageText, identity }) {
    const sessions = new Map();
    const cache = new Map();
    const amount = (value, time = false) => {
      if (!value || value.exact == null && !value.measured) return '—';
      const format = time ? durationMs : compactNumber;
      return `${value.exact == null ? '≥ ' : ''}${format(value.measured)}`;
    };
    const exact = value => value ? `${Number(value.measured).toLocaleString(window.DevCoordinatorI18n.locale)} measured tokens${value.exact == null ? '; incomplete measurement' : ''}` : 'Measurement unavailable';
    const badge = disposition => `<span class="performance-status ${esc(disposition)}">${window.DevCoordinatorI18n.computedMarkup(() => statuses[disposition] ? window.DevCoordinatorI18n.t("performance.status_" + disposition) : disposition)}</span>`;
    const color = key => ({coding:'var(--chart-blue)',integration_testing:'var(--chart-violet)',unknown:'var(--muted)',unattributed:'var(--muted)'})[key] || `var(--chart-${['blue', 'violet', 'amber', 'teal', 'green'][Array.from(key).reduce((n, c) => n + c.charCodeAt(0), 0) % 5]})`;
    const entries = values => Object.entries(values || {}).sort((a, b) => b[1].measured - a[1].measured || a[0].localeCompare(b[0]));
    function mix(values, total) {
      if (!total) return "<span class=\"muted\"><span data-i18n=\"performance.no_measured_tokens_bba758\">No measured tokens</span></span>";
      return `<span class="performance-stack" role="img" aria-label="" ${window.DevCoordinatorI18n.computedAttribute("aria-label", () => entries(values).map(([key, value]) => `${activityLabel(key)}: ${exact(value)}`).join('; '))}>${entries(values).map(([key, value]) => `<span style="width:${Math.max(0, Math.min(100, value.measured / total * 100))}%;background:${key === 'unattributed' || key === 'unknown' ? 'var(--muted)' : color(key)}" title="${esc(`${kindLabel(key)}: ${exact(value)}`)}"></span>`).join('')}</span>`;
    }
    function distribution(title, values, total, kind = false) {
      const rows = entries(values); const sum = rows.reduce((n, [, value]) => n + value.measured, 0);
      return `<section class="performance-distribution"><div class="performance-section-head"><h2>${window.DevCoordinatorI18n.markup("performance." + title)}</h2><span><span data-i18n="performance.total_c9b3c3">Total</span> <strong title="${esc(exact(total))}">${window.DevCoordinatorI18n.computedMarkup(() => amount(total))}</strong></span></div>${mix(values, sum)}<div class="performance-legend">${rows.slice(0, 5).map(([key, value]) => `<span><i style="background:${key === 'unattributed' ? 'var(--muted)' : color(key)}"></i>${esc(kind ? labels[key] || activityLabel(key) : activityLabel(key))}<strong>${window.DevCoordinatorI18n.computedMarkup(() => amount(value))}</strong></span>`).join('')}${rows.length > 5 ? `<span>${window.DevCoordinatorI18n.markup("performance.value1_more_activities_a0186b", {value1: rows.length - 5})}</span>` : ''}</div><details class="performance-exact"><summary><span data-i18n="performance.exact_values_29bdd9">Exact values</span></summary><table><thead><tr><th>${(kind ? window.DevCoordinatorI18n.markup("performance.plan_item_kind_4accd6") : window.DevCoordinatorI18n.markup("performance.activity_38da15"))}</th><th><span data-i18n="performance.measured_tokens_ca4840">Measured tokens</span></th><th><span data-i18n="performance.share_29887a">Share</span></th></tr></thead><tbody>${rows.map(([key, value]) => `<tr><td>${esc(kind ? labels[key] || activityLabel(key) : activityLabel(key))}</td><td>${window.DevCoordinatorI18n.formatted('number', Number(value.measured))}${(value.exact == null ? window.DevCoordinatorI18n.markup("performance.partial_27f7a4") : '')}</td><td>${sum ? (value.measured / sum * 100).toFixed(1) + '%' : '—'}</td></tr>`).join('')}</tbody></table></details></section>`;
    }
    async function render(root, repositoryId, signal) {
      if (!identity()?.administrator) { root.innerHTML = "<h1><a class=\"destination-link\" href=\"#/performance\"><span data-i18n=\"performance.performance_442ade\">Performance</span></a></h1><p class=\"notice\"><span data-i18n=\"performance.administrator_access_is_required_to_read_perform_81e663\">Administrator access is required to read performance reviews.</span></p>"; return; }
      if (!repositoryId) { root.innerHTML = "<h1><a class=\"destination-link\" href=\"#/performance\"><span data-i18n=\"performance.performance_442ade\">Performance</span></a></h1><p><span data-i18n=\"performance.no_repository_is_selected_386f0c\">No repository is selected.</span></p>"; return; }
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
      const requests = { repository_id: repositoryId, window_start_ms: start, window_end_ms: end, outcome_limit: 5 };
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
        target.innerHTML = `<p role="alert">${esc(message)}</p><button type="button" class="btn btn-small"><span data-i18n="performance.retry_942087">Retry</span></button>`;
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
      root.innerHTML = `<section data-performance-page><div class="performance-top"><header class="performance-heading"><h1><a class="destination-link" href="#/performance"><span data-i18n="performance.performance_442ade">Performance</span></a></h1><div class="performance-range" aria-label="Overview period" data-i18n-attrs='{"aria-label":"performance.overview_period_80309e"}'>${['7d', '30d', '90d'].map(value => `<button type="button" class="btn btn-small" data-performance-range="${value}" aria-pressed="${range === value}">${value}</button>`).join('')}<button type="button" class="btn btn-small" data-performance-custom-open><span data-i18n="performance.custom_494ca7">Custom</span></button><dialog class="performance-date-dialog"><form><h2><span data-i18n="performance.custom_dates_9c855c">Custom dates</span></h2><label><span data-i18n="performance.from_utc_bec5d5">From (UTC)</span><input type="date" name="from" value="${day(start)}" required></label><label><span data-i18n="performance.through_utc_615e9f">Through (UTC)</span><input type="date" name="to" value="${day(end - 1)}" max="${day(Date.now())}" required></label><p role="alert"></p><button class="btn btn-primary" type="submit"><span data-i18n="performance.apply_dates_eb4a4d">Apply dates</span></button><button class="btn" type="button" data-performance-cancel-dates><span data-i18n="performance.cancel_19766e">Cancel</span></button></form></dialog></div><span class="performance-dates">${window.DevCoordinatorI18n.computedMarkup(() => period(start, end))}</span><button type="button" class="btn btn-small" data-performance-refresh><span data-i18n="performance.refresh_0e9161">Refresh</span></button></header>
        <div class="performance-totals" aria-label="Repository totals" data-i18n-attrs='{"aria-label":"performance.repository_totals_3058a4"}'><div><span><span data-i18n="performance.all_recorded_tokens_998a8f">All recorded tokens</span></span><strong data-performance-lifetime><span data-i18n="performance.loading_ba3bbb">Loading…</span></strong></div><div><span><span data-i18n="performance.overview_period_tokens_9bcdf5">Overview period tokens</span></span><strong data-performance-period-total><span data-i18n="performance.loading_ba3bbb">Loading…</span></strong></div><div><span><span data-i18n="performance.all_reviews_ab84bf">All reviews</span></span><strong data-performance-count><span data-i18n="performance.loading_ba3bbb">Loading…</span></strong></div></div></div>
        <div class="performance-scope"><div><h2 data-performance-scope-title>${(reference ? window.DevCoordinatorI18n.markup("performance.review_usage_c39675") : window.DevCoordinatorI18n.markup("performance.overview_usage_83f004"))}</h2><span data-performance-scope-total></span></div>${reference ? "<button type=\"button\" class=\"btn btn-small\" data-performance-overview><span data-i18n=\"performance.return_to_overview_d8eeb2\">Return to overview</span></button>" : ''}</div><p class="performance-coverage muted" data-performance-coverage role="status"><span data-i18n="performance.loading_measurements_baea02">Loading measurements…</span></p>
        <div class="performance-distributions" data-performance-charts><p><span data-i18n="performance.loading_token_breakdowns_b62cda">Loading token breakdowns…</span></p></div>
        <div class="performance-workspace"><div class="performance-primary"><section class="performance-outcomes"><div class="performance-section-head"><h2><span data-i18n="performance.tokens_by_outcome_860e7c">Tokens by outcome</span></h2><span data-performance-outcome-count></span></div><div data-performance-outcomes><span data-i18n="performance.loading_outcomes_a12af3">Loading outcomes…</span></div><div data-performance-outcomes-more></div></section>
        <section class="performance-history"><h2><span data-i18n="performance.review_history_063ea2">Review history</span></h2><div data-performance-history><span data-i18n="performance.loading_reviews_510c76">Loading reviews…</span></div><div data-performance-history-more></div></section></div>
        <aside class="performance-reader" data-performance-reader tabindex="-1" aria-label="Selected performance review" data-i18n-attrs='{"aria-label":"performance.selected_performance_review_362d6a"}'>${reference ? "<p><span data-i18n=\"performance.loading_review_fc2441\">Loading review…</span></p>" : "<h2><span data-i18n=\"performance.review_details_1640b0\">Review details</span></h2><p class=\"muted\"><span data-i18n=\"performance.select_a_review_to_see_its_decisions_measurement_316280\">Select a review to see its decisions, measurements and evidence.</span></p>"}</aside></div></section>`;
      root.querySelectorAll('[data-performance-range]').forEach(button => button.addEventListener('click', () => { const nextEnd = Date.now(); navigate(null, { range: button.dataset.performanceRange, from: String(nextEnd - parseInt(button.dataset.performanceRange, 10) * DAY), to: String(nextEnd) }); }));
      find('overview')?.addEventListener('click', () => navigate(null));
      find('refresh').addEventListener('click', () => { cache.clear(); session.history = null; session.periodUsage = null; session.end = Date.now(); session.lifetimeEnd = Date.now(); window.render(); });
      const form = root.querySelector('.performance-date-dialog form');
      const custom = root.querySelector('.performance-date-dialog');
      root.append(custom);
      find('custom-open').addEventListener('click', () => custom.showModal());
      custom.addEventListener('close', () => find('custom-open')?.focus());
      find('cancel-dates').addEventListener('click', () => custom.close());
      form.addEventListener('submit', event => { event.preventDefault(); const from = Date.parse(form.elements.from.value + 'T00:00:00Z'); const to = Math.min(Date.parse(form.elements.to.value + 'T00:00:00Z') + DAY, Date.now()); if (!Number.isFinite(from) || !Number.isFinite(to) || from < 0 || from >= to) { window.DevCoordinatorI18n.text(form.querySelector('[role=alert]'), "performance.choose_a_start_date_on_or_before_the_end_date_89966f"); return; } navigate(null, { range: 'custom', from: String(from), to: String(to) }); });
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
      const showCoverage = usage => { window.DevCoordinatorI18n.bind(find('coverage'), () => `${coverageText(usage.coverage)}${usage.outcomes.unattributed.operations ? ' · ' + window.DevCoordinatorI18n.t("performance.unattributedWork") : ''}`); };
      function showUsage(usage, append = false) {
        const previousUsage = currentUsage; currentUsage = usage;
        if (!reference) { session.periodUsage = { start, end, usage: append && previousUsage ? { ...usage, outcomes: { ...usage.outcomes, rows: [...previousUsage.outcomes.rows, ...usage.outcomes.rows] } } : usage }; currentUsage = session.periodUsage.usage; }
        const outcomes = usage.outcomes; const total = outcomes.totals.providerTotalTokens;
        if (!append) {
          showCoverage(usage);
          window.DevCoordinatorI18n.bind(find('scope-total'), () => window.DevCoordinatorI18n.t(reference ? "performance.reviewScopeTotal" : "performance.periodScopeTotal", {tokens: amount(total)}) + (reference ? '' : ' · ' + period(start, end)));
          find('charts').innerHTML = distribution('tokensByActivity', outcomes.totals.activities, total) + distribution('tokensByKind', outcomes.kinds, total, true);
          find('outcomes').innerHTML = `<table class="performance-outcome-table"><thead><tr><th><span data-i18n="performance.outcome_4e80ab">Outcome</span></th><th><span data-i18n="performance.plan_item_kind_4accd6">Plan item kind</span></th><th><span data-i18n="performance.activity_mix_3b0dfb">Activity mix</span></th><th><span data-i18n="performance.total_tokens_e7601c">Total tokens</span></th></tr></thead><tbody></tbody><tfoot><tr><th colspan="3">${window.DevCoordinatorI18n.markup(reference ? "performance.reviewTotalLabel" : "performance.periodTotalLabel")}</th><td>${window.DevCoordinatorI18n.computedMarkup(() => amount(total))}</td></tr></tfoot></table>`;
        }
        if (usage.coverage.state === 'unavailable' && !total.measured) find('charts').innerHTML = "<p class=\"muted\"><span data-i18n=\"performance.token_measurements_are_unavailable_for_this_peri_cd0465\">Token measurements are unavailable for this period.</span></p>";
        const body = find('outcomes').querySelector('tbody');
        const unassigned = { outcomeId: null, title: null, unattributed: true, effort: outcomes.unattributed };
        const rows = [...outcomes.rows, ...(!append && (outcomes.unattributed.operations || outcomes.unattributed.providerTotalTokens.measured) ? [unassigned] : [])];
        body.insertAdjacentHTML('beforeend', rows.map(row => `<tr><td data-label="Outcome" data-i18n-attrs='{"data-label":"performance.outcome_4e80ab"}'>${row.outcomeId && row.title ? `<a href="#/plan/${encodeURIComponent(repositoryId)}?task=${encodeURIComponent(row.outcomeId)}">${esc(row.title)}</a>` : (row.unattributed ? window.DevCoordinatorI18n.markup("performance.kind_unattributed") : row.title ? esc(row.title) : window.DevCoordinatorI18n.markup("performance.outcome_no_longer_available_a63b43"))}</td><td data-label="Plan item kind" data-i18n-attrs='{"data-label":"performance.plan_item_kind_4accd6"}'>${row.outcomeId ? (labels[row.kind] ? window.DevCoordinatorI18n.computedMarkup(() => kindLabel(row.kind)) : window.DevCoordinatorI18n.markup("performance.unknown_kind_2a60d8")) : esc('—')}</td><td data-label="Activity mix" data-i18n-attrs='{"data-label":"performance.activity_mix_3b0dfb"}'>${mix(row.effort.activities, row.effort.providerTotalTokens.measured)}</td><td data-label="Total tokens" title="${esc(exact(row.effort.providerTotalTokens))}" data-i18n-attrs='{"data-label":"performance.total_tokens_e7601c"}'>${window.DevCoordinatorI18n.computedMarkup(() => amount(row.effort.providerTotalTokens))}</td></tr>`).join(''));
        if (!rows.length && !append) body.innerHTML = "<tr><td colspan=\"4\"><span data-i18n=\"performance.no_measured_outcomes_for_this_period_e905c6\">No measured outcomes for this period.</span></td></tr>";
        window.DevCoordinatorI18n.bind(find('outcome-count'), () => window.DevCoordinatorI18n.t("performance.value1_attributed_outcomes_6e3dca", {value1: outcomes.totalRows}));
        find('outcomes-more').innerHTML = outcomes.nextCursor ? "<button type=\"button\" class=\"btn btn-small\"><span data-i18n=\"performance.load_more_outcomes_408c45\">Load more outcomes</span></button>" : '';
        find('outcomes-more').querySelector('button')?.addEventListener('click', async event => {
          event.currentTarget.disabled = true;
          try { const data = reference ? await read('performance.review', { repository_id: repositoryId, reference, outcome_cursor: currentUsage.outcomes.nextCursor, outcome_limit: 5 }, false) : await read('performance.overview', { ...requests, outcome_cursor: currentUsage.outcomes.nextCursor }, false); if (active()) showUsage(data.usage, true); }
          catch (error) { if (active()) errorPanel(find('outcomes-more'), error.message, () => { cache.clear(); window.render(); }); }
        });
      }
      function evidenceMarkup(item) {
        const ref = item.source; let href = null;
        if (item.available && ref.kind === 'outcome') href = `#/plan/${encodeURIComponent(repositoryId)}?task=${encodeURIComponent(ref.reference)}`;
        if (item.available && ref.kind === 'decision') href = `#/decisions/${encodeURIComponent(repositoryId)}?q=${encodeURIComponent(ref.reference)}`;
        if (item.available && ref.kind === 'run' && ref.reference.includes('/')) { const [worktree, run] = ref.reference.split('/'); href = `#/tests?repository=${encodeURIComponent(repositoryId)}&run=${encodeURIComponent(run)}&worktree=${encodeURIComponent(worktree)}`; }
        return `<li>${item.available && ref.kind === 'release' ? `<button class="btn btn-small" type="button" data-performance-release="${esc(ref.reference)}"><span data-i18n="performance.view_delivery_evidence_c18555">View delivery evidence</span></button>` : ''}${href ? `<a href="${esc(href)}">${esc(item.title)}</a>` : `<strong>${esc(item.title)}</strong>`}${!item.available ? "<span class=\"muted\"> <span data-i18n=\"performance.evidence_unavailable_492fe7\">· Evidence unavailable</span></span>" : ''}${item.body ? `<p>${esc(item.body)}</p>` : ''}</li>`;
      }
      function showReview(data) {
        reviewData = data; const r = data.review; const e = r.record.experiment;
        window.DevCoordinatorI18n.bind(find('scope-title'), () => window.DevCoordinatorI18n.t("performance.review_usage_value1_a681eb", {value1: period(r.record.windowStartMs, r.record.windowEndMs)}));
        showUsage(data.usage);
        const text = (title, value) => value ? `<h3>${window.DevCoordinatorI18n.computedMarkup(() => window.DevCoordinatorI18n.label(title))}</h3><p>${esc(value)}</p>` : '';
        find('reader').innerHTML = `<div class="performance-section-head"><h2>${window.DevCoordinatorI18n.computedMarkup(() => date(r.recorded_at_ms))} · Review</h2>${badge(e.disposition)}</div><p class="muted">Revision ${r.revision}${(r.completed ? '' : window.DevCoordinatorI18n.markup("performance.review_in_progress_bf2daa"))}</p>${text('Decision', e.chosenAction)}
          <section class="performance-comparison"><h3><span data-i18n="performance.performance_comparison_fc2574">Performance comparison</span></h3>${data.comparisons.length ? data.comparisons.map(comparison => `<div>${comparison.verified_improvement ? "<strong class=\"performance-gain\"><span data-i18n=\"performance.verified_improvement_70cd1e\">Verified improvement</span></strong>" : "<strong><span data-i18n=\"performance.comparison_not_verified_as_an_improvement_63ee70\">Comparison not verified as an improvement</span></strong>"}<p class="muted">${window.DevCoordinatorI18n.computedMarkup(() => period(comparison.window_start_ms, comparison.window_end_ms))}</p>${comparison.metrics.map(metric => { const time = metric.metric !== 'tokens'; const title = { tokens: 'Tokens', active_agent_ms: 'Active agent time', elapsed_execution_ms: 'Elapsed execution time', recorded_wait_ms: 'Recorded waiting time' }[metric.metric]; const max = Math.max(metric.before.measured, metric.after.measured, 1); return `<div class="performance-metric"><h4>${title}</h4><div class="performance-before-after">${[['Before', metric.before], ['After', metric.after]].map(([label, value]) => `<div><span>${label}</span><strong>${window.DevCoordinatorI18n.computedMarkup(() => amount(value, time))}</strong><meter min="0" max="${max}" value="${value.measured}" aria-label="${esc(`${title} ${label.toLowerCase()}: ${amount(value, time)}`)}"></meter></div>`).join('')}</div><p class="${metric.reduction > 0 && comparison.verified_improvement ? 'performance-gain' : 'muted'}">${(metric.reduction == null ? window.DevCoordinatorI18n.markup("performance.incomplete_measurement_3407ad") : `${metric.reduction > 0 ? 'Reduction' : metric.reduction < 0 ? 'Increase' : 'No change'}${metric.reduction ? ': ' + esc(time ? durationMs(Math.abs(metric.reduction)) : compactNumber(Math.abs(metric.reduction))) : ''}${metric.reduction_percent == null ? '' : ` (${Math.abs(metric.reduction_percent).toFixed(1)}%)`}`)}</p></div>`; }).join('')}<p class="muted">${esc(comparison.explanation)}</p></div>`).join('') : "<p class=\"muted\"><span data-i18n=\"performance.no_verified_before_and_after_measurements_are_re_8de459\">No verified before-and-after measurements are recorded for this review.</span></p>"}</section>
          <details open><summary><span data-i18n="performance.reasoning_and_alternatives_db7b7b">Reasoning and alternatives</span></summary>${text('Hypothesis', e.hypothesis)}<h3><span data-i18n="performance.alternatives_considered_73534c">Alternatives considered</span></h3><ul>${e.alternatives.map(value => `<li>${esc(value)}</li>`).join('')}</ul>${text('Rationale', e.reason)}${text('Baseline', e.baseline.interpretation)}${text('Comparison', e.comparison?.narrative)}${text('Changed inputs', e.comparison?.inputChangeReason)}${text('Success criteria', e.successCriteria)}${text('Revert condition', e.rollbackCondition)}${e.observations.map(o => text(activityLabel(o.kind), o.interpretation)).join('')}</details>
          <details><summary><span data-i18n="performance.evidence_and_measurement_coverage_5fff3b">Evidence and measurement coverage</span></summary><ul class="performance-evidence">${data.evidence.map(evidenceMarkup).join('')}</ul>${e.baseline.missingMeasurements.length ? `<h3><span data-i18n="performance.recorded_measurement_gaps_503313">Recorded measurement gaps</span></h3><ul>${e.baseline.missingMeasurements.map(gap => `<li>${window.DevCoordinatorI18n.computedMarkup(() => activityLabel(gap))}</li>`).join('')}</ul>` : ''}<p>${esc(data.usage.outcomes.basis)}</p></details>
          <details data-performance-revisions><summary><span data-i18n="performance.revision_history_5ae0d3">Revision history</span></summary><div data-performance-revision-list></div><div data-performance-revision-more></div></details>`;
        find('revisions').addEventListener('toggle', () => { if (find('revisions').open && !find('revision-list').children.length) loadRevisions(); });
        find('reader').querySelectorAll('[data-performance-release]').forEach(button => button.addEventListener('click', async () => {
          button.disabled = true;
          try { const receipt = await api('release.evidence', { reference: button.dataset.performanceRelease });
            if (active() && button.isConnected) { const info = document.createElement('p'); info.textContent = receipt.target + ' · ' + (receipt.qualified ? 'Verified delivery' : 'Verification pending') + (receipt.verified_at_ms ? ' · ' + date(receipt.verified_at_ms) : '');
              if (receipt.access && /^https?:\/\//.test(receipt.access)) { const link = document.createElement('a'); link.href = receipt.access; window.DevCoordinatorI18n.text(link, "performance.open_delivered_result_93d4e9"); info.append(' · ', link); } button.after(info); button.remove(); }
          } catch (error) { if (active() && button.isConnected) { button.disabled = false; button.parentElement.querySelector('[role=alert]')?.remove(); const message = document.createElement('p'); message.setAttribute('role','alert'); window.DevCoordinatorI18n.bind(message, () => error.message); button.after(message); } }
        }));
        placeReader();
        if (session.focusReview) { window.scrollTo({ top: session.activationScroll || 0 }); find('reader').focus({ preventScroll: true }); if (matchMedia('(max-width: 1000px)').matches) find('reader').querySelector('h2').scrollIntoView({ block: 'nearest' }); session.focusReview = false; }
      }
      async function loadRevisions(more = false) {
        try { const data = await read('performance.reviews', { repository_id: repositoryId, record_id: reviewData.review.record_id, ...(more ? { before: revisionBefore } : {}) }); if (!active()) return; revisionBefore = data.next_before;
          find('revision-list').insertAdjacentHTML('beforeend', data.records.map(({ review }) => `<p><a href="${esc(route(review.reference))}"${review.reference === reference ? ' aria-current="true"' : ''}>Revision ${review.revision} · ${window.DevCoordinatorI18n.computedMarkup(() => date(review.recorded_at_ms))}</a> ${badge(review.record.experiment.disposition)}</p>`).join(''));
          find('revision-more').innerHTML = revisionBefore ? "<button class=\"btn btn-small\" type=\"button\"><span data-i18n=\"performance.earlier_revisions_e54f55\">Earlier revisions</span></button>" : ''; find('revision-more').querySelector('button')?.addEventListener('click', () => loadRevisions(true));
        } catch (error) { if (active()) errorPanel(find('revision-list'), error.message, () => loadRevisions()); }
      }
      async function loadHistory(more = false) {
        try { const data = !more && session.history ? session.history : await read('performance.reviews', { repository_id: repositoryId, ...(more ? { before: historyBefore } : {}) }); if (!active()) return; historyBefore = data.next_before; session.history = more && session.history ? { ...data, records: [...session.history.records, ...data.records] } : data; find('count').textContent = String(data.total_reviews);
          if (!more) find('history').innerHTML = data.records.length ? "<table><thead><tr><th><span data-i18n=\"performance.date_99c40a\">Date</span></th><th><span data-i18n=\"performance.review_decision_beaf31\">Review decision</span></th><th><span data-i18n=\"performance.status_920e41\">Status</span></th></tr></thead><tbody></tbody></table>" : "<p class=\"muted\"><span data-i18n=\"performance.no_performance_reviews_have_been_recorded_for_th_5e849a\">No performance reviews have been recorded for this repository.</span></p>";
          find('history').querySelector('tbody')?.insertAdjacentHTML('beforeend', data.records.map(({ review, revision_count }) => `<tr${review.record_id === reference?.split('@')[0] ? ' class="selected"' : ''}><td>${window.DevCoordinatorI18n.computedMarkup(() => date(review.recorded_at_ms))}</td><td><a data-performance-select="${esc(review.reference)}" href="${esc(route(review.reference))}">${esc(review.record.experiment.chosenAction.length > 140 ? review.record.experiment.chosenAction.slice(0, 137) + "…" : review.record.experiment.chosenAction)}</a>${revision_count > 1 ? `<small>${window.DevCoordinatorI18n.markup("performance.value1_revisions_ed2971", {value1: revision_count})}</small>` : ''}</td><td>${badge(review.record.experiment.disposition)}</td></tr>`).join(''));
          placeReader();
          find('history-more').innerHTML = historyBefore ? "<button type=\"button\" class=\"btn btn-small\"><span data-i18n=\"performance.load_earlier_reviews_48a029\">Load earlier reviews</span></button>" : ''; find('history-more').querySelector('button')?.addEventListener('click', event => { event.currentTarget.disabled = true; loadHistory(true); });
        } catch (error) { if (active()) { if (!more) window.DevCoordinatorI18n.text(find('count'), "performance.unavailable_ca1844"); errorPanel(more ? find('history-more') : find('history'), error.message, () => loadHistory(more)); } }
      }
      root.addEventListener('click', event => { if (event.target.closest('[data-performance-select]')) { session.focusReview = true; session.activationScroll = window.scrollY; if (!reference) session.scroll = window.scrollY; } }, { signal });
      async function loadPeriod() {
        try { const data = await read('performance.overview', requests); if (!active()) return; find('period-total').textContent = amount(data.total_tokens); if (!reference) { showUsage(session.periodUsage?.start === start && session.periodUsage?.end === end ? session.periodUsage.usage : data.usage); if (session.scroll) window.scrollTo({ top: session.scroll }); } }
        catch (error) { if (active()) { window.DevCoordinatorI18n.text(find('period-total'), "performance.unavailable_ca1844"); if (!reference) { window.DevCoordinatorI18n.text(find('coverage'), "performance.measurements_could_not_be_loaded_77193b"); window.DevCoordinatorI18n.text(find('outcomes'), "performance.measurements_could_not_be_loaded_77193b"); errorPanel(find('charts'), error.message, loadPeriod); } } }
      }
      async function loadLifetime() {
        try { const data = await read('performance.overview', { repository_id: repositoryId, window_start_ms: 0, window_end_ms: session.lifetimeEnd, totals_only: true }); if (!active()) return; find('lifetime').textContent = amount(data.total_tokens); find('lifetime').title = coverageText(data.coverage); }
        catch (error) { if (active()) { window.DevCoordinatorI18n.text(find('lifetime'), "performance.unavailable_ca1844"); find('lifetime').title = error.message; } }
      }
      async function loadReview() {
        try { const data = await read('performance.review', { repository_id: repositoryId, reference, outcome_limit: 5 }); if (active()) showReview(data); }
        catch (error) { if (active()) { errorPanel(find('reader'), error.message, loadReview); window.DevCoordinatorI18n.text(find('coverage'), "performance.review_measurements_are_unavailable_497d4e"); find('charts').innerHTML = ''; window.DevCoordinatorI18n.text(find('outcomes'), "performance.review_measurements_are_unavailable_497d4e"); } }
      }
      await Promise.allSettled([loadHistory(), loadPeriod(), loadLifetime(), ...(reference ? [loadReview()] : [])]);
      if (active()) root.style.minHeight = '';
      signal.addEventListener('abort', () => { root.style.minHeight = ''; }, { once: true });
    }
    return { render };
  }
  return { create };
})();
