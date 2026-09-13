'use strict';

window.DevCoordinatorTests = (() => {
  const activeStates = new Set(['running', 'pending', 'queued']);
  const timestamp = (run) => Date.parse(run.started_at || '') || 0;

  function groupRuns(runs) {
    const groups = new Map();
    for (const run of runs) {
      const key = run.repository_source?.key || run.repository_id || run.worktree_id;
      if (!groups.has(key)) groups.set(key, { key, name: run.repository_source?.name || run.display_name, runs: [] });
      groups.get(key).runs.push(run);
    }
    for (const group of groups.values()) group.runs.sort((left, right) => timestamp(right) - timestamp(left) || left.run_id.localeCompare(right.run_id));
    return [...groups.values()].sort((left, right) => timestamp(right.runs[0]) - timestamp(left.runs[0]) || left.name.localeCompare(right.name));
  }

  async function render({ main, runs: initialRuns, capacity, retention, api, esc, badge, durationMs, bytes, signal, openLogs, openFiles, openCapacity, openRetention, bindSettings, repository = null }) {
    let runs = initialRuns;
    let groups = groupRuns(runs);
    let selected;
    try { selected = sessionStorage.getItem('dc2-tests-repository'); } catch {}
    if (!groups.some((group) => group.key === selected)) selected = groups[0]?.key;
    const cache = new Map();
    const urls = new Set();
    let observer;
    let refreshTimer;
    let refreshPromise;
    let previewDialog;
    const current = () => repository ? { name: repository.name, runs: runs.filter((run) => repository.ids.includes(run.repository_id)).sort((left, right) => timestamp(right) - timestamp(left) || left.run_id.localeCompare(right.run_id)) } : groups.find((group) => group.key === selected);
    const query = (selector) => main.querySelector(selector);
    const readable = (name) => {
      const label = String(name || 'Test').replace(/[-_]+/g, ' ').replace(/\bmacos\b/gi, 'macOS').replace(/\bui\b/g, 'UI');
      return label.startsWith('macOS') ? label : label.charAt(0).toUpperCase() + label.slice(1);
    };
    const viewerUrl = (run, image) => `#/tests/${encodeURIComponent(run.run_id)}?${new URLSearchParams({ ...(image ? { image: image.image_id } : {}), ...(run.worktree_id ? { worktree: run.worktree_id } : {}) })}`;
    const duration = (run) => durationMs(run.duration_seconds == null ? null : run.duration_seconds * 1000);
    const time = (run) => Number.isFinite(Date.parse(run.started_at)) ? new Date(run.started_at).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }) : 'Time unavailable';
    const availableFiles = (run) => window.DevCoordinatorArtifacts.bundles(run);
    const evidenceRun = (run) => run.visual_evidence?.status === 'available' || run.visual_evidence?.issue_count ? run
      : run.earlier_visual_evidence ? { ...run, ...run.earlier_visual_evidence, earlier: true } : null;

    const checkState = (check) => {
      const execution = check.execution;
      if (!activeStates.has(check.status) || !execution) return check.status;
      if (execution.executing) return 'running';
      if (execution.admitted) return 'starting';
      return execution.waiting ? 'waiting' : check.status;
    };
    const checkTiming = (check) => {
      if (!check.execution) return durationMs(check.duration_seconds == null ? null : check.duration_seconds * 1000);
      const execution = check.execution;
      if (activeStates.has(check.status)) return [execution.executing ? `${execution.executing} executing` : '', execution.waiting ? `${execution.waiting} waiting` : ''].filter(Boolean).join(' · ');
      return `${durationMs(execution.process_duration_ms)} process time · ${durationMs(execution.capacity_wait_ms)} waiting`;
    };
    const checkRows = (checks) => checks.map((check) => `<div class="test-check"><span>${esc(readable(check.name))}</span>${badge(checkState(check))}<span class="muted">${esc(checkTiming(check))}</span></div>`).join('');
    const reportIssue = (issue) => ({
      missing: 'The run ended without a check report.',
      invalid: 'The check report could not be validated. The run logs are available for diagnosis.',
      unreadable: 'The check report could not be read. The run logs are available for diagnosis.',
      identity_mismatch: 'The check report belongs to a different run and was rejected.',
      incomplete: 'The run ended before all checks reported their results.',
    })[issue] || '';

    function row(run) {
      const visual = evidenceRun(run);
      const files = availableFiles(run);
      return `<article class="test-result" data-test-run-id="${esc(run.run_id)}">
        <div class="test-result-summary"><span class="test-status-icon ${run.status === 'failed' ? 'bad' : ''}"><span class="ti ti-${run.status === 'passed' ? 'circle-check' : run.status === 'failed' ? 'circle-x' : 'refresh'}" aria-hidden="true"></span></span><div class="test-result-name"><h2>${esc(readable(run.test))}</h2><div class="test-run-time"><time datetime="${esc(run.started_at)}" title="${esc(run.started_at)}">${esc(time(run))}</time>${run.duration_seconds == null ? '' : `<span>· ${esc(duration(run))}</span>`}</div></div>${badge(run.status)}</div>
        ${visual || files.length ? `<div class="test-previews" data-preview-run="${esc(run.run_id)}" aria-label="Screenshots for ${esc(readable(run.test))}"><span class="muted">Loading screenshots…</span></div>` : ''}
        <div class="test-result-actions"><button type="button" class="test-text-action" data-test-logs>Logs</button>${files.length || run.checks_truncated ? '<button type="button" class="test-text-action" data-test-artifacts>Files</button>' : ''}<button type="button" class="test-text-action ${activeStates.has(run.status) ? 'test-stop' : ''}" data-test-start>${activeStates.has(run.status) ? 'Stop run' : 'Run again'}</button><details class="test-detail" data-disclosure="${esc(run.run_id)}"><summary aria-label="Details for ${esc(readable(run.test))}">Details</summary><div class="test-detail-content"><div data-test-checks>${checkRows(run.checks || [])}</div><dl class="test-technical"><dt>Checkout</dt><dd>${esc(run.worktree_path)}</dd><dt>Validation</dt><dd>${esc(run.requested_tier || 'Not recorded')}</dd><dt>Exit code</dt><dd>${run.exit_code ?? '—'}</dd><dt>Output / errors</dt><dd>${bytes(run.stdout_bytes_observed)} / ${bytes(run.stderr_bytes_observed)}</dd></dl>${!files.length && !run.checks_truncated ? '<button type="button" class="test-text-action" data-test-artifacts>Earlier files</button>' : ''}</div></details></div><div class="test-action-error" role="status">${esc(reportIssue(run.report_issue))}</div>
      </article>`;
    }

    async function action(button, run) {
      const error = button.closest('.test-result').querySelector('.test-action-error');
      button.disabled = true;
      error.textContent = '';
      try {
        await api(activeStates.has(run.status) ? 'test.stop' : 'test.start', { path: run.worktree_path, ...(activeStates.has(run.status) ? {} : { test: run.test, tier: run.requested_tier || 'release' }) }, false);
        if (!signal.aborted) await refresh();
      } catch (failure) {
        if (!signal.aborted) error.textContent = failure.message;
      } finally {
        button.disabled = false;
      }
    }

    async function imageUrl(run, image) {
      const chunks = [];
      let offset = 0;
      let total = null;
      do {
        if (signal.aborted) throw new Error('Screenshot loading cancelled.');
        const chunk = await api('test.evidence.image', { path: run.worktree_path, run_id: run.run_id, image_id: image.image_id, offset, max_bytes: 184320 });
        const block = Uint8Array.from(atob(chunk.base64 || ''), (character) => character.charCodeAt(0));
        if (chunk.image_id !== image.image_id || chunk.offset !== offset || block.length !== chunk.bytes || chunk.total_bytes > 16777216 || (total != null && chunk.total_bytes !== total)) throw new Error('The screenshot changed while loading.');
        total = chunk.total_bytes;
        chunks.push(block);
        offset += block.length;
        if (!block.length || (chunk.next_offset != null && chunk.next_offset !== offset)) throw new Error('The screenshot response was incomplete.');
        if (chunk.next_offset == null && offset !== total) throw new Error('The screenshot response was incomplete.');
      } while (offset < total);
      if (signal.aborted) throw new Error('Screenshot loading cancelled.');
      const url = URL.createObjectURL(new Blob(chunks, { type: 'image/png' }));
      urls.add(url);
      return url;
    }

    async function ensureImage(result, image) {
      if (image.url) return image.url;
      if (!image.loading) image.loading = (image.load ? image.load() : imageUrl(result.run, image)).then((url) => { image.url = url; return url; }).catch((error) => { image.loading = null; throw error; });
      return image.loading;
    }

    function preview(result, initialIndex, opener) {
      previewDialog?.dispatchEvent(new Event('cancel', { cancelable: true }));
      const { run, images } = result;
      let index = initialIndex;
      let generation = 0;
      const dialog = document.createElement('dialog');
      previewDialog = dialog;
      dialog.className = 'test-image-preview';
      dialog.setAttribute('aria-label', 'Screenshot gallery');
      const rowId = opener.closest('[data-test-run-id]')?.dataset.testRunId;
      const thumbnailIndex = [...opener.parentElement.querySelectorAll('.test-thumbnail')].indexOf(opener);
      dialog.innerHTML = `<div class="dialog-head"><div><span data-gallery-title></span><small data-gallery-count aria-live="polite"></small>${run.earlier ? `<small class="test-preview-provenance">Earlier run · ${esc(time(run))}</small>` : ''}</div><button type="button" class="dialog-close" aria-label="Close screenshot preview"><span class="ti ti-x" aria-hidden="true"></span></button></div><div class="test-gallery-stage"><button type="button" class="test-gallery-arrow" data-gallery-previous aria-label="Previous screenshot"${images.length === 1 ? ' disabled' : ''}><span class="ti ti-chevron-left" aria-hidden="true"></span></button><div class="test-gallery-image"><img data-gallery-image data-ui-continuation-anchor alt=""><div class="test-gallery-message" role="status"></div></div><button type="button" class="test-gallery-arrow" data-gallery-next aria-label="Next screenshot"${images.length === 1 ? ' disabled' : ''}><span class="ti ti-chevron-right" aria-hidden="true"></span></button></div><nav class="test-gallery-thumbnails" aria-label="Screenshots">${images.map((image, position) => `<button type="button" data-gallery-index="${position}" aria-label="Screenshot ${position + 1}: ${esc(image.label)}" aria-pressed="false"><img alt="" width="96" height="64"><span>${esc(image.label)}</span></button>`).join('')}</nav><footer data-gallery-footer></footer>`;
      const rail = dialog.querySelector('.test-gallery-thumbnails');
      const thumbnails = [...rail.querySelectorAll('button')];
      const railObserver = new IntersectionObserver((entries) => {
        for (const entry of entries) if (entry.isIntersecting) {
          railObserver.unobserve(entry.target);
          const image = images[Number(entry.target.dataset.galleryIndex)];
          ensureImage(result, image).then((url) => { if (dialog.isConnected) entry.target.querySelector('img').src = url; }).catch(() => {});
        }
      }, { root: rail, rootMargin: '100px' });
      const close = () => {
        generation += 1;
        railObserver.disconnect();
        dialog.close(); dialog.remove(); if (previewDialog === dialog) previewDialog = null;
        const restored = opener.isConnected ? opener : thumbnailIndex < 0 ? query(`[data-test-run-id="${CSS.escape(rowId || '')}"] .test-preview-more`) : query(`[data-test-run-id="${CSS.escape(rowId || '')}"]`)?.querySelectorAll('.test-thumbnail')[thumbnailIndex];
        restored?.focus({ preventScroll: true });
      };
      const show = async (position) => {
        index = (position + images.length) % images.length;
        const requested = ++generation;
        const image = images[index];
        const display = dialog.querySelector('[data-gallery-image]');
        const message = dialog.querySelector('.test-gallery-message');
        dialog.querySelector('[data-gallery-title]').textContent = image.label;
        dialog.querySelector('[data-gallery-count]').textContent = `${index + 1} of ${images.length}`;
        display.alt = image.label;
        display.hidden = !image.url;
        if (image.url) display.src = image.url;
        message.textContent = image.url ? '' : 'Loading screenshot…';
        thumbnails.forEach((button, position) => button.setAttribute('aria-pressed', String(position === index)));
        thumbnails[index].scrollIntoView({ block: 'nearest', inline: 'nearest' });
        const footer = dialog.querySelector('[data-gallery-footer]');
        footer.innerHTML = image.native ? '<button type="button" class="btn" data-open-file>Open file</button>' : `<a class="btn" href="${viewerUrl(run, image)}">Open in viewer / comment</a>`;
        footer.querySelector('[data-open-file]')?.addEventListener('click', () => { close(); openFiles(run, opener, image); });
        try {
          const url = await ensureImage(result, image);
          if (requested !== generation || !dialog.isConnected || signal.aborted) return;
          display.src = url;
          await display.decode();
          if (requested !== generation || !dialog.isConnected || signal.aborted) return;
          display.hidden = false;
          thumbnails[index].querySelector('img').src = url;
          message.textContent = '';
        } catch (error) {
          if (requested !== generation || !dialog.isConnected || signal.aborted) return;
          display.hidden = true;
          message.innerHTML = `<span>${esc(error.message)}</span><button type="button" class="btn btn-small">Retry screenshot</button>`;
          message.querySelector('button').addEventListener('click', () => { image.url = null; image.loading = null; show(index); });
        }
      };
      dialog.addEventListener('cancel', (event) => { event.preventDefault(); close(); });
      dialog.querySelector('.dialog-close').addEventListener('click', close);
      dialog.querySelector('[data-gallery-previous]').addEventListener('click', () => show(index - 1));
      dialog.querySelector('[data-gallery-next]').addEventListener('click', () => show(index + 1));
      thumbnails.forEach((button, position) => button.addEventListener('click', () => show(position)));
      dialog.addEventListener('keydown', (event) => {
        const position = { ArrowLeft: index - 1, ArrowRight: index + 1, Home: 0, End: images.length - 1 }[event.key];
        if (position == null || event.altKey || event.ctrlKey || event.metaKey) return;
        event.preventDefault(); show(position);
        if (rail.contains(event.target)) thumbnails[index].focus({ preventScroll: true });
      });
      document.body.appendChild(dialog);
      dialog.showModal();
      thumbnails.forEach((button) => railObserver.observe(button));
      show(index);
    }

    async function loadImages(run) {
      const visual = evidenceRun(run);
      if (!visual) return window.DevCoordinatorArtifacts.previews(run, { api, signal, urls });
      const data = await api('test.evidence.get', { path: visual.worktree_path, run_id: visual.run_id });
      const candidates = [...new Map((data.bundles || []).flatMap((bundle) => (bundle.cells || []).flatMap((cell) => ['viewport', 'full_page'].flatMap((kind) => {
        const screenshot = cell.screenshots?.[kind];
        return screenshot?.status === 'available' ? [{ ...screenshot, label: `${readable(cell.state_name)} · ${cell.viewport?.name || 'Screenshot'}${kind === 'full_page' ? ' · full page' : ''}` }] : [];
      }))).map((image) => [image.image_id, image])).values()];
      if (!candidates.length && data.issues?.length) throw new Error('Screenshots could not be read.');
      return { run: visual, images: candidates, count: candidates.length };
    }

    async function populatePreviews(host, run) {
      const key = JSON.stringify([run.run_id, run.visual_evidence, run.earlier_visual_evidence, availableFiles(run)]);
      if (!cache.has(key)) cache.set(key, loadImages(run).catch((error) => { cache.delete(key); throw error; }));
      try {
        const result = await cache.get(key);
        const preferred = result.images.filter((image) => image.kind !== 'full-page');
        const initialImages = [...preferred, ...result.images.filter((image) => !preferred.includes(image))].slice(0, 4);
        await Promise.all(initialImages.map(async (image) => {
          const probe = new Image();
          probe.src = await ensureImage(result, image);
          try { await probe.decode(); } catch { throw new Error('Screenshot could not be previewed.'); }
        }));
        if (signal.aborted || !host.isConnected) return;
        host.replaceChildren();
        if (!result.images.length) { host.hidden = true; return; }
        for (const image of initialImages) {
          const button = document.createElement('button');
          button.type = 'button'; button.className = 'test-thumbnail';
          button.setAttribute('aria-label', `Preview ${image.label}`);
          button.innerHTML = `<img src="${esc(image.url)}" alt="${esc(image.label)}" width="112" height="76"><span>${esc(image.label)}</span>`;
          button.addEventListener('click', () => preview(result, result.images.indexOf(image), button));
          host.append(button);
        }
        if (result.count > initialImages.length) {
          const more = document.createElement('button');
          more.type = 'button'; more.className = 'test-preview-more';
          more.textContent = `+${result.count - initialImages.length}`;
          more.setAttribute('aria-label', `Browse all ${result.count} screenshots`);
          more.addEventListener('click', () => preview(result, result.images.findIndex((image) => !initialImages.includes(image)), more));
          host.append(more);
        }
        if (result.run.earlier) {
          const provenance = document.createElement('span');
          provenance.className = 'test-preview-provenance';
          provenance.textContent = `Earlier run · ${time(result.run)}`;
          provenance.title = 'These screenshots do not verify the latest run.';
          host.append(provenance);
        }
      } catch (error) {
        if (signal.aborted || !host.isConnected) return;
        cache.delete(key);
        host.innerHTML = `<span class="muted">${esc(error.message)}</span><button class="test-text-action" type="button">Retry screenshots</button>`;
        host.querySelector('button').addEventListener('click', () => populatePreviews(host, run));
      }
    }

    function paintRows() {
      observer?.disconnect();
      const collection = query('#test-runs-collection');
      const form = query('#test-run-form');
      const focusedField = form?.contains(document.activeElement) ? document.activeElement : null;
      const focusedRow = document.activeElement.closest('[data-test-run-id]')?.dataset.testRunId;
      const focusedAction = ['data-test-logs', 'data-test-start', 'data-test-artifacts'].find((attribute) => document.activeElement.hasAttribute(attribute));
      const open = new Set([...collection.querySelectorAll('details[open]')].map((details) => details.dataset.disclosure));
      const scroll = { top: scrollY, left: scrollX };
      const group = current();
      collection.setAttribute('aria-label', group ? `${group.name} test results` : 'Test results');
      collection.innerHTML = group?.runs.length ? `<div class="test-results">${group.runs.map(row).join('')}</div>` : '<p class="muted">No test runs yet.</p>';
      if (form) { collection.prepend(form); focusedField?.focus({ preventScroll: true }); }
      query('#test-run-open').disabled = !group?.runs.some((run) => !activeStates.has(run.status));
      for (const article of collection.querySelectorAll('.test-result')) {
        const run = group.runs.find((item) => item.run_id === article.dataset.testRunId);
        for (const button of article.querySelectorAll('[data-test-logs], [data-test-artifacts]')) button.dataset.runId = run.run_id;
        article.querySelector('[data-test-logs]').addEventListener('click', (event) => openLogs(run, event.currentTarget));
        for (const button of article.querySelectorAll('[data-test-artifacts]')) button.addEventListener('click', () => openFiles(run, button));
        article.querySelector('[data-test-start]').addEventListener('click', (event) => action(event.currentTarget, run));
        const details = article.querySelector('details');
        details.addEventListener('toggle', async () => {
          if (!details.open || !run.checks_truncated || details.dataset.loading) return;
          details.dataset.loading = 'true';
          try {
            const detail = await api('test.status', { path: run.worktree_path });
            if (signal.aborted || !details.isConnected) return;
            if (detail.run_id !== run.run_id) throw new Error('A newer run is available. Refresh to view it.');
            details.querySelector('[data-test-checks]').innerHTML = checkRows(detail.checks || []);
          } catch (error) { if (!signal.aborted && details.isConnected) article.querySelector('.test-action-error').textContent = error.message; }
          finally { delete details.dataset.loading; }
        });
        details.open = open.has(run.run_id);
      }
      observer = new IntersectionObserver((entries) => {
        for (const entry of entries) if (entry.isIntersecting) {
          observer.unobserve(entry.target);
          const run = group.runs.find((item) => item.run_id === entry.target.dataset.previewRun);
          populatePreviews(entry.target, run);
        }
      }, { rootMargin: '160px' });
      for (const host of collection.querySelectorAll('[data-preview-run]')) observer.observe(host);
      if (focusedRow && focusedAction) query(`[data-test-run-id="${CSS.escape(focusedRow)}"] [${focusedAction}]`)?.focus({ preventScroll: true });
      window.scrollTo(scroll);
    }

    function paintNavigation() {
      const navigation = query('.test-repository-list');
      if (!navigation) return;
      const names = groups.map((group) => group.name);
      navigation.innerHTML = groups.map((group) => `<button type="button" class="test-repository" data-repository-key="${esc(group.key)}" aria-pressed="${group.key === selected}">${esc(group.name)}${names.filter((name) => name === group.name).length > 1 ? `<small>${esc(group.runs[0].display_name === group.name ? group.runs[0].worktree_path : group.runs[0].display_name)}</small>` : ''}</button>`).join('');
      for (const button of navigation.querySelectorAll('button')) button.addEventListener('click', () => {
        selected = button.dataset.repositoryKey;
        try { sessionStorage.setItem('dc2-tests-repository', selected); } catch {}
        query('#test-run-form')?.remove();
        for (const choice of navigation.querySelectorAll('button')) choice.setAttribute('aria-pressed', String(choice === button));
        paintRows();
        query('#test-runs-collection').focus({ preventScroll: true });
        window.scrollTo({ top: 0, left: 0 });
      });
    }

    function runForm(opener) {
      if (query('#test-run-form')) { query('#test-run-form').remove(); return; }
      const choices = current()?.runs.filter((run) => !activeStates.has(run.status)) || [];
      const form = document.createElement('form');
      form.id = 'test-run-form';
      form.innerHTML = `<label class="f">Test<select name="run">${choices.map((run) => `<option value="${esc(run.run_id)}">${esc(readable(run.test))} · ${esc(time(run))}</option>`).join('')}</select></label><label class="f">Validation<select name="tier"><option value="release">Release</option><option value="pre-merge">Pre-merge</option><option value="development">Development</option></select></label><div class="test-run-form-actions"><button type="submit" class="btn btn-primary">Run</button><button class="test-text-action" type="button" data-cancel>Cancel</button></div><p role="status"></p>`;
      form.querySelector('[data-cancel]').addEventListener('click', () => { form.remove(); opener.focus(); });
      form.addEventListener('keydown', (event) => { if (event.key === 'Escape') { event.preventDefault(); form.remove(); opener.focus(); } });
      form.addEventListener('submit', async (event) => {
        event.preventDefault();
        event.submitter.disabled = true;
        try {
          const fields = new FormData(form);
          const run = choices.find((choice) => choice.run_id === fields.get('run'));
          await api('test.start', { path: run.worktree_path, test: run.test, tier: fields.get('tier') }, false);
          form.remove(); opener.focus();
          if (!signal.aborted) await refresh();
        } catch (error) { form.querySelector('[role=status]').textContent = error.message; event.submitter.disabled = false; }
      });
      query('#test-runs-collection').prepend(form);
      form.querySelector('select').focus({ preventScroll: true });
      form.scrollIntoView({ block: 'nearest' });
    }

    async function refresh() {
      if (signal.aborted) return;
      if (refreshPromise) return refreshPromise;
      clearTimeout(refreshTimer);
      refreshPromise = (async () => {
        try {
          const next = (await api('test.list', {})).runs || [];
          if (signal.aborted) return;
          if (JSON.stringify(next) !== JSON.stringify(runs)) {
            runs = next; groups = groupRuns(runs);
            if (!current()) selected = groups[0]?.key;
            paintNavigation(); paintRows();
          }
          query('#test-live-status').textContent = '';
        } catch (error) {
          if (!signal.aborted) query('#test-live-status').textContent = `Updates paused: ${error.message}`;
        } finally {
          refreshPromise = null;
          if (!signal.aborted && runs.some((run) => activeStates.has(run.status))) refreshTimer = setTimeout(refresh, 2000);
        }
      })();
      return refreshPromise;
    }

    main.innerHTML = repository ? `<section class="tests-workspace"><header class="workspace-tests-heading"><h1>Tests</h1><button class="btn" type="button" id="test-run-open">Run tests</button></header><span id="test-live-status" role="status"></span><section id="test-runs-collection" tabindex="-1"></section></section>` : `<section class="tests-workspace"><aside class="tests-sidebar"><header><h1><a href="#/tests" class="destination-link">Tests</a></h1><details class="test-settings"><summary class="test-settings-toggle" aria-label="Test settings"><span class="ti ti-settings" aria-hidden="true"></span></summary><div class="test-settings-menu"><button class="btn" type="button" id="test-log-retention-open">Log retention</button><button class="btn" type="button" id="test-capacity-open">Capacity</button></div></details></header><nav class="test-repository-list" aria-label="Repositories"></nav><button class="btn" type="button" id="test-run-open">Run tests</button><span id="test-live-status" role="status"></span></aside><section id="test-runs-collection" tabindex="-1"></section></section>`;
    paintNavigation(); paintRows(); bindSettings();
    query('#test-run-open').addEventListener('click', (event) => runForm(event.currentTarget));
    query('#test-capacity-open')?.addEventListener('click', (event) => openCapacity(capacity, event.currentTarget));
    query('#test-log-retention-open')?.addEventListener('click', (event) => openRetention(retention, event.currentTarget));
    signal.addEventListener('abort', () => {
      clearTimeout(refreshTimer); observer?.disconnect();
      previewDialog?.dispatchEvent(new Event('cancel', { cancelable: true }));
      for (const url of urls) URL.revokeObjectURL(url);
      urls.clear(); cache.clear();
    }, { once: true });
    if (runs.some((run) => activeStates.has(run.status))) refreshTimer = setTimeout(refresh, 2000);
  }

  return { groupRuns, render };
})();
