'use strict';
// One visible status for the owned reads of a view. Query progress is distinct
// from a producer's publication backlog; the latter has no completion estimate.
window.DevCoordinatorUsageLoading = (() => {
  const pendingReasons = new Set(['api_refresh_pending', 'api_source_busy', 'api_busy']);
  const pending = coverage => Boolean(coverage?.loading) || Object.keys(coverage?.unavailable_reasons || {}).some(reason => pendingReasons.has(reason));
  const markup = () => '<span class="usage-loading-status" data-usage-loading role="status" aria-live="polite" aria-atomic="true" hidden><span class="ti ti-refresh usage-loading-spinner" aria-hidden="true"></span><span data-usage-loading-label></span><span class="usage-loading-estimate" data-usage-loading-estimate></span></span>';
  function create({ target, signal }) {
    const reads = new Map();
    let timer = null;
    const text = (key, params) => window.DevCoordinatorI18n.t('common.' + key, params);
    function paint() {
      clearTimeout(timer); timer = null;
      if (signal.aborted) return;
      const host = target();
      const now = performance.now();
      const entries = [...reads.values()];
      const oldest = entries.reduce((at, row) => Math.min(at, row.started), now);
      const visible = entries.length > 0 && now - oldest >= 1000;
      if (host) {
        host.hidden = !visible;
        if (visible) {
          const title = entries.every(row => row.history) ? 'usageLoadingReviews' : 'usageLoadingTokens';
          const remaining = entries.map(row => {
            const value = row.loading?.estimated_remaining_ms;
            return row.loading?.state === 'reading' && row.loading.estimate_samples >= 3
              && row.loading.estimate_basis === 'recent_comparable_source_reads'
              && Number.isFinite(value) && value > 0 ? value - Number(row.loading.elapsed_ms || 0) : null;
          });
          const overrun = entries.some(row => row.loading?.state === 'estimate_exceeded') || remaining.some(value => value != null && value <= 0);
          let estimate = text(overrun ? 'usageEstimateExceeded' : 'usageEstimateUnknown');
          if (!overrun && remaining.every(value => value != null)) {
            const ms = Math.max(...remaining);
            estimate = text(ms < 60000 ? 'usageEstimateSeconds' : 'usageEstimateMinutes', { count: ms < 60000 ? Math.max(5, Math.ceil(ms / 5000) * 5) : Math.ceil(ms / 60000) });
          }
          const label = host.querySelector('[data-usage-loading-label]');
          const detail = host.querySelector('[data-usage-loading-estimate]');
          if (label.textContent !== text(title)) label.textContent = text(title);
          if (detail.textContent !== estimate) detail.textContent = estimate;
        }
      }
      if (entries.length) timer = setTimeout(paint, visible ? 1000 : Math.max(1, 1000 - (now - oldest)));
    }
    function begin(key, history = false) {
      if (signal.aborted) return;
      if (!reads.has(key)) reads.set(key, { started: performance.now(), observed: performance.now(), loading: null, history });
      paint();
    }
    function update(key, coverages) {
      if (signal.aborted) return;
      const waiting = (Array.isArray(coverages) ? coverages : [coverages]).filter(pending);
      const previous = reads.get(key);
      if (!waiting.length) reads.delete(key);
      else {
        const known = waiting.map(coverage => coverage.loading);
        // Unknown or queued work prevents a numeric estimate for the whole read.
        const loading = known.some(value => !value || value.state === 'waiting_for_source') ? null
          : known.find(value => value.state === 'estimate_exceeded')
            || (known.every(value => value.estimated_remaining_ms != null) ? known.reduce((a, b) => a.estimated_remaining_ms >= b.estimated_remaining_ms ? a : b) : null);
        reads.set(key, { started: previous?.started ?? performance.now(), observed: performance.now(), loading, history: previous?.history ?? false });
      }
      paint();
    }
    function finish(key) { reads.delete(key); paint(); }
    signal.addEventListener('abort', () => { clearTimeout(timer); reads.clear(); }, { once: true });
    return { begin, update, finish, paint };
  }
  return { create, markup, pending };
})();
