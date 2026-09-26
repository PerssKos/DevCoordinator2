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
    const replacementNotices = new Map();
    const runTargets = (run) => run.targets?.length ? { targets: run.targets } : { test: run.test };
    const runSelection = (run) => ({ ...runTargets(run), ...(run.selection?.length ? { checks: run.selection } : {}), ...(Object.keys(run.case_selection || {}).length ? { cases: run.case_selection } : {}) });
    const recordReplacement = (run, result) => {
      if (result.superseded_run_id) replacementNotices.set(run.worktree_path, `Previous run ${result.superseded_run_id} was cancelled by this start.`);
      else replacementNotices.delete(run.worktree_path);
      for (const article of main.querySelectorAll('[data-test-run-id]')) {
        const displayed = runs.find((item) => item.run_id === article.dataset.testRunId);
        if (displayed?.worktree_path === run.worktree_path) window.DevCoordinatorI18n.bind(article.querySelector('.test-replacement-notice'), () => (replacementNotices.get(run.worktree_path) || ''));
      }
    };
    const current = () => repository ? { name: repository.name, runs: runs.filter((run) => repository.ids.includes(run.repository_id)).sort((left, right) => timestamp(right) - timestamp(left) || left.run_id.localeCompare(right.run_id)) } : groups.find((group) => group.key === selected);
    const query = (selector) => main.querySelector(selector);
    const readable = (name) => {
      const label = String(name || 'Test').replace(/[-_]+/g, ' ').replace(/\bmacos\b/gi, 'macOS').replace(/\bui\b/g, 'UI');
      return label.startsWith('macOS') ? label : label.charAt(0).toUpperCase() + label.slice(1);
    };
    const viewerUrl = (run, image) => `#/tests/${encodeURIComponent(run.run_id)}?${new URLSearchParams({ ...(image ? { image: image.image_id } : {}), ...(run.worktree_id ? { worktree: run.worktree_id } : {}) })}`;
    const duration = (run) => durationMs(run.duration_seconds == null ? null : run.duration_seconds * 1000);
    const time = (run) => Number.isFinite(Date.parse(run.started_at)) ? new Date(run.started_at).toLocaleString(window.DevCoordinatorI18n.locale, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }) : 'Time unavailable';
    const availableFiles = (run) => window.DevCoordinatorArtifacts.bundles(run);
    const evidenceRun = (run) => run.visual_evidence?.status === 'available' || run.visual_evidence?.issue_count ? run
      : run.earlier_visual_evidence ? { ...run, ...run.earlier_visual_evidence, earlier: true } : null;

    const checkState = (check) => {
      if (check.resource_waiting) return 'waiting';
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
    const checkBadge = (check) => ['invalidated', 'not_meaningful'].includes(check.status) ? "<span class=\"badge\"><span data-i18n=\"tests.not_run_25f0c2\">Not run</span></span>" : badge(checkState(check));
    const runLabel = (run) => run.targets?.length ? run.targets.map(readable).join(' + ') : readable(run.test);
    const selectionLabel = (run) => run.proof === 'selected' ? 'Selected checks' : run.proof === 'retry' ? 'Retry' : '';
    const phaseTime = (ms) => ms > 0 && ms < 1000 ? `${Math.max(1, Math.round(ms))}ms` : durationMs(ms);
    const phaseTimes = (run) => run.phase_durations?.some((entry) => entry.phase !== 'check') ? `<table class="test-phase-times" aria-label="Phase durations" data-i18n-attrs='{"aria-label":"tests.phase_durations_e500c2"}'><thead><tr><th><span data-i18n="tests.phase_46342e">Phase</span></th><th><span data-i18n="tests.execution_a45cd4">Execution</span></th><th><span data-i18n="tests.elapsed_a194a6">Elapsed</span></th></tr></thead><tbody>${run.phase_durations.map((entry) => `<tr><th scope="row">${esc(readable(entry.phase))}</th><td>${window.DevCoordinatorI18n.computedMarkup(() => phaseTime(entry.duration_seconds * 1000))}</td><td>${(entry.elapsed_seconds == null ? window.DevCoordinatorI18n.markup("tests.unavailable_ca1844") : esc(phaseTime(entry.elapsed_seconds * 1000)))}</td></tr>`).join('')}</tbody></table>` : '';
    const caseRows = (check) => (check.cases || []).filter((item) => item.phases?.length).map((item) => `<div class="test-case"><div class="test-case-heading"><strong>${esc(item.id)}</strong>${badge(item.status)}</div><ul>${item.phases.map((phase) => `<li class="test-case-phase"><span>${(phase.phase === 'fixture' ? window.DevCoordinatorI18n.markup("tests.database_setup_7a9e87") : (phase.phase === 'case' ? window.DevCoordinatorI18n.markup("tests.execution_a45cd4") : esc(readable(phase.phase))))}</span>${phase.status === 'invalidated' ? "<span class=\"badge\"><span data-i18n=\"tests.not_run_25f0c2\">Not run</span></span>" : badge(phase.status)}<span>${phase.status === 'invalidated' ? '—' : esc(phaseTime(phase.duration_ms))}</span></li>`).join('')}</ul></div>`).join('');
    const checkRows = (checks) => checks.map((check) => `<div class="test-check-group"><div class="test-check"><span>${esc(check.display_name || readable(check.name))}</span>${checkBadge(check)}<span class="muted">${window.DevCoordinatorI18n.computedMarkup(() => checkTiming(check))}</span></div>${caseRows(check)}${check.cases_truncated ? `<p class="muted">${window.DevCoordinatorI18n.markup("tests.value1_of_value2_cases_shown_all_retained_case_o_89db76", {value1: check.cases?.length || 0, value2: check.case_count})}</p>` : ''}</div>`).join('');
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
        <div class="test-result-summary"><span class="test-status-icon ${run.status === 'failed' ? 'bad' : ''}"><span class="ti ti-${run.status === 'passed' ? 'circle-check' : run.status === 'failed' ? 'circle-x' : 'refresh'}" aria-hidden="true"></span></span><div class="test-result-name"><h2>${esc(runLabel(run))}</h2><div class="test-run-time"><time datetime="${esc(run.started_at)}" title="${esc(run.started_at)}">${window.DevCoordinatorI18n.computedMarkup(() => time(run))}</time>${run.duration_seconds == null ? '' : `<span>· ${window.DevCoordinatorI18n.computedMarkup(() => duration(run))}</span>`}${selectionLabel(run) ? `<span>${window.DevCoordinatorI18n.computedMarkup(() => selectionLabel(run))}</span>` : ''}</div></div>${badge(run.status)}</div>
        ${visual || files.length ? `<div class="test-previews" data-preview-run="${esc(run.run_id)}" aria-label="Screenshots for ${esc(runLabel(run))}"><span class="muted"><span data-i18n="tests.loading_screenshots_e06f72">Loading screenshots…</span></span></div>` : ''}
        <div class="test-result-actions"><button type="button" class="test-text-action" data-test-logs><span data-i18n="tests.logs_ea2100">Logs</span></button>${files.length || run.checks_truncated ? "<button type=\"button\" class=\"test-text-action\" data-test-artifacts><span data-i18n=\"tests.files_abc7e9\">Files</span></button>" : ''}<button type="button" class="test-text-action ${activeStates.has(run.status) ? 'test-stop' : ''}" data-test-start>${activeStates.has(run.status) ? window.DevCoordinatorI18n.markup("tests.stop_run_b7ec68") : window.DevCoordinatorI18n.markup("tests.run_again_3e310b")}</button><details class="test-detail" data-disclosure="${esc(run.run_id)}"><summary aria-label="Details for ${esc(runLabel(run))}"><span data-i18n="tests.details_45989d">Details</span></summary><div class="test-detail-content"><div data-test-checks>${checkRows(run.checks || [])}</div>${phaseTimes(run)}<dl class="test-technical"><dt><span data-i18n="tests.checkout_99e71f">Checkout</span></dt><dd>${esc(run.worktree_path)}</dd><dt><span data-i18n="tests.validation_68e1ca">Validation</span></dt><dd>${run.requested_tier ? esc(run.requested_tier) : window.DevCoordinatorI18n.markup("tests.not_recorded_b37c78")}</dd><dt><span data-i18n="tests.exit_code_ccc6eb">Exit code</span></dt><dd>${run.exit_code ?? '—'}</dd><dt><span data-i18n="tests.output_errors_878776">Output / errors</span></dt><dd>${window.DevCoordinatorI18n.computedMarkup(() => bytes(run.stdout_bytes_observed))} / ${window.DevCoordinatorI18n.computedMarkup(() => bytes(run.stderr_bytes_observed))}</dd></dl>${!files.length && !run.checks_truncated ? "<button type=\"button\" class=\"test-text-action\" data-test-artifacts><span data-i18n=\"tests.earlier_files_0fd1b6\">Earlier files</span></button>" : ''}</div></details></div><div class="test-action-error" role="status">${esc(reportIssue(run.report_issue))}</div>
        <p class="test-replacement-notice" role="status">${esc(replacementNotices.get(run.worktree_path) || '')}</p>
      </article>`;
    }

    async function action(button, run) {
      const error = button.closest('.test-result').querySelector('.test-action-error');
      button.disabled = true;
      error.textContent = '';
      try {
        const result = await api(activeStates.has(run.status) ? 'test.stop' : 'test.start', { path: run.worktree_path, ...(activeStates.has(run.status) ? {} : { ...runSelection(run), tier: run.requested_tier || 'release' }) }, false);
        recordReplacement(run, result);
        if (!signal.aborted) await refresh();
      } catch (failure) {
        if (!signal.aborted) window.DevCoordinatorI18n.bind(error, () => failure.message);
      } finally {
        button.disabled = false;
      }
    }

    async function imageUrl(run, image) {
      const chunks = [];
      let offset = 0;
      let total = null;
      do {
        if (signal.aborted) throw window.DevCoordinatorI18n.error("tests.screenshot_loading_cancelled_50e74f");
        const chunk = await api('test.evidence.image', { path: run.worktree_path, run_id: run.run_id, image_id: image.image_id, offset, max_bytes: 184320 });
        const block = Uint8Array.from(atob(chunk.base64 || ''), (character) => character.charCodeAt(0));
        if (chunk.image_id !== image.image_id || chunk.offset !== offset || block.length !== chunk.bytes || chunk.total_bytes > 16777216 || (total != null && chunk.total_bytes !== total)) throw window.DevCoordinatorI18n.error("tests.the_screenshot_changed_while_loading_94fe13");
        total = chunk.total_bytes;
        chunks.push(block);
        offset += block.length;
        if (!block.length || (chunk.next_offset != null && chunk.next_offset !== offset)) throw window.DevCoordinatorI18n.error("tests.the_screenshot_response_was_incomplete_4d7952");
        if (chunk.next_offset == null && offset !== total) throw window.DevCoordinatorI18n.error("tests.the_screenshot_response_was_incomplete_4d7952");
      } while (offset < total);
      if (signal.aborted) throw window.DevCoordinatorI18n.error("tests.screenshot_loading_cancelled_50e74f");
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
      window.DevCoordinatorI18n.text(dialog, "tests.screenshot_gallery_7bd298", {}, "aria-label");
      const rowId = opener.closest('[data-test-run-id]')?.dataset.testRunId;
      const thumbnailIndex = [...opener.parentElement.querySelectorAll('.test-thumbnail')].indexOf(opener);
      dialog.innerHTML = `<div class="dialog-head"><div><span data-gallery-title></span><small data-gallery-count aria-live="polite"></small>${run.earlier ? `<small class="test-preview-provenance">Earlier run · ${window.DevCoordinatorI18n.computedMarkup(() => time(run))}</small>` : ''}</div><button type="button" class="dialog-close" aria-label="Close screenshot preview" data-i18n-attrs='{"aria-label":"tests.close_screenshot_preview_e49000"}'><span class="ti ti-x" aria-hidden="true"></span></button></div><div class="test-gallery-stage"><button type="button" class="test-gallery-arrow" data-gallery-previous aria-label="Previous screenshot"${images.length === 1 ? ' disabled' : ''} data-i18n-attrs='{"aria-label":"tests.previous_screenshot_af9be1"}'><span class="ti ti-chevron-left" aria-hidden="true"></span></button><div class="test-gallery-image"><img data-gallery-image data-ui-continuation-anchor alt=""><div class="test-gallery-message" role="status"></div></div><button type="button" class="test-gallery-arrow" data-gallery-next aria-label="Next screenshot"${images.length === 1 ? ' disabled' : ''} data-i18n-attrs='{"aria-label":"tests.next_screenshot_fd250f"}'><span class="ti ti-chevron-right" aria-hidden="true"></span></button></div><nav class="test-gallery-thumbnails" aria-label="Screenshots" data-i18n-attrs='{"aria-label":"tests.screenshots_067348"}'>${images.map((image, position) => `<button type="button" data-gallery-index="${position}" aria-label="Screenshot ${position + 1}: ${esc(image.label)}" aria-pressed="false"><img alt="" width="96" height="64"><span>${esc(image.label)}</span></button>`).join('')}</nav><footer data-gallery-footer></footer>`;
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
        window.DevCoordinatorI18n.bind(dialog.querySelector('[data-gallery-count]'), () => window.DevCoordinatorI18n.t("tests.value1_of_value2_882c45", {value1: index + 1, value2: images.length}));
        display.alt = image.label;
        display.hidden = !image.url;
        if (image.url) display.src = image.url;
        window.DevCoordinatorI18n.bind(message, () => (image.url ? '' : window.DevCoordinatorI18n.t("tests.loading_screenshot_e0fd7b")));
        thumbnails.forEach((button, position) => button.setAttribute('aria-pressed', String(position === index)));
        thumbnails[index].scrollIntoView({ block: 'nearest', inline: 'nearest' });
        const footer = dialog.querySelector('[data-gallery-footer]');
        footer.innerHTML = image.native ? "<button type=\"button\" class=\"btn\" data-open-file><span data-i18n=\"tests.open_file_4190c0\">Open file</span></button>" : `<a class="btn" href="${viewerUrl(run, image)}"><span data-i18n="tests.open_in_viewer_comment_ad9420">Open in viewer / comment</span></a>`;
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
          message.innerHTML = `<span>${esc(error.message)}</span><button type="button" class="btn btn-small"><span data-i18n="tests.retry_screenshot_23b6c7">Retry screenshot</span></button>`;
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
      if (!candidates.length && data.issues?.length) throw window.DevCoordinatorI18n.error("tests.screenshots_could_not_be_read_92f171");
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
          try { await probe.decode(); } catch { throw window.DevCoordinatorI18n.error("tests.screenshot_could_not_be_previewed_11a0c5"); }
        }));
        if (signal.aborted || !host.isConnected) return;
        host.replaceChildren();
        if (!result.images.length) { host.hidden = true; return; }
        for (const image of initialImages) {
          const button = document.createElement('button');
          button.type = 'button'; button.className = 'test-thumbnail';
          window.DevCoordinatorI18n.bind(button, () => window.DevCoordinatorI18n.t("tests.preview_value1_cb5dab", {value1: image.label}), "aria-label");
          button.innerHTML = `<img src="${esc(image.url)}" alt="${esc(image.label)}" width="112" height="76"><span>${esc(image.label)}</span>`;
          button.addEventListener('click', () => preview(result, result.images.indexOf(image), button));
          host.append(button);
        }
        if (result.count > initialImages.length) {
          const more = document.createElement('button');
          more.type = 'button'; more.className = 'test-preview-more';
          more.textContent = `+${result.count - initialImages.length}`;
          window.DevCoordinatorI18n.bind(more, () => window.DevCoordinatorI18n.t("tests.browse_all_value1_screenshots_81fcf6", {value1: result.count}), "aria-label");
          more.addEventListener('click', () => preview(result, result.images.findIndex((image) => !initialImages.includes(image)), more));
          host.append(more);
        }
        if (result.run.earlier) {
          const provenance = document.createElement('span');
          provenance.className = 'test-preview-provenance';
          window.DevCoordinatorI18n.bind(provenance, () => window.DevCoordinatorI18n.t("tests.earlier_run_value1_68bdba", {value1: time(result.run)}));
          window.DevCoordinatorI18n.text(provenance, "tests.these_screenshots_do_not_verify_the_latest_run_0e24e3", {}, "title");
          host.append(provenance);
        }
      } catch (error) {
        if (signal.aborted || !host.isConnected) return;
        cache.delete(key);
        host.innerHTML = `<span class="muted">${esc(error.message)}</span><button class="test-text-action" type="button"><span data-i18n="tests.retry_screenshots_232a99">Retry screenshots</span></button>`;
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
      window.DevCoordinatorI18n.bind(collection, () => (group ? window.DevCoordinatorI18n.t("tests.value1_test_results_293210", {value1: group.name}) : window.DevCoordinatorI18n.t("tests.test_results_3ea600")), "aria-label");
      collection.innerHTML = group?.runs.length ? `<div class="test-results">${group.runs.map(row).join('')}</div>` : "<p class=\"muted\"><span data-i18n=\"tests.no_test_runs_yet_3c5be5\">No test runs yet.</span></p>";
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
            if (detail.run_id !== run.run_id) throw window.DevCoordinatorI18n.error("tests.a_newer_run_is_available_refresh_to_view_it_c113b3");
            details.querySelector('[data-test-checks]').innerHTML = checkRows(detail.checks || []);
          } catch (error) { if (!signal.aborted && details.isConnected) window.DevCoordinatorI18n.bind(article.querySelector('.test-action-error'), () => error.message); }
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
      form.innerHTML = `<label class="f"><span data-i18n="tests.test_532eaa">Test</span><select name="run">${choices.map((run) => `<option value="${esc(run.run_id)}">${esc(runLabel(run))} · ${esc(time(run))}</option>`).join('')}</select></label><label class="f"><span data-i18n="tests.validation_68e1ca">Validation</span><select name="tier"><option value="release" data-i18n="tests.release_e020e3">Release</option><option value="pre-merge" data-i18n="tests.pre_merge_175f55">Pre-merge</option><option value="development" data-i18n="tests.development_21b6a7">Development</option></select></label><div class="test-run-form-actions"><button type="submit" class="btn btn-primary"><span data-i18n="tests.run_00d60e">Run</span></button><button class="test-text-action" type="button" data-cancel><span data-i18n="tests.cancel_19766e">Cancel</span></button></div><p role="status"></p>`;
      form.querySelector('[data-cancel]').addEventListener('click', () => { form.remove(); opener.focus(); });
      form.addEventListener('keydown', (event) => { if (event.key === 'Escape') { event.preventDefault(); form.remove(); opener.focus(); } });
      form.addEventListener('submit', async (event) => {
        event.preventDefault();
        event.submitter.disabled = true;
        try {
          const fields = new FormData(form);
          const run = choices.find((choice) => choice.run_id === fields.get('run'));
          const result = await api('test.start', { path: run.worktree_path, ...runTargets(run), tier: fields.get('tier') }, false);
          recordReplacement(run, result);
          form.remove(); opener.focus();
          if (!signal.aborted) await refresh();
        } catch (error) { window.DevCoordinatorI18n.bind(form.querySelector('[role=status]'), () => error.message); event.submitter.disabled = false; }
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
          if (!signal.aborted) window.DevCoordinatorI18n.bind(query('#test-live-status'), () => window.DevCoordinatorI18n.t("tests.updates_paused_value1_11b351", {value1: error.message}));
        } finally {
          refreshPromise = null;
          if (!signal.aborted && runs.some((run) => activeStates.has(run.status))) refreshTimer = setTimeout(refresh, 2000);
        }
      })();
      return refreshPromise;
    }

    main.innerHTML = repository ? `<section class="tests-workspace"><header class="workspace-tests-heading"><h1><span data-i18n="tests.tests_e5c9d7">Tests</span></h1><button class="btn" type="button" id="test-run-open"><span data-i18n="tests.run_tests_3f6d6b">Run tests</span></button></header><span id="test-live-status" role="status"></span><section id="test-runs-collection" tabindex="-1"></section></section>` : `<section class="tests-workspace"><aside class="tests-sidebar"><header><h1><a href="#/tests" class="destination-link"><span data-i18n="tests.tests_e5c9d7">Tests</span></a></h1><details class="test-settings"><summary class="test-settings-toggle" aria-label="Test settings" data-i18n-attrs='{"aria-label":"tests.test_settings_1db6e5"}'><span class="ti ti-settings" aria-hidden="true"></span></summary><div class="test-settings-menu"><button class="btn" type="button" id="test-log-retention-open"><span data-i18n="tests.log_retention_46c385">Log retention</span></button><button class="btn" type="button" id="test-capacity-open"><span data-i18n="tests.capacity_ae65d0">Capacity</span></button></div></details></header><nav class="test-repository-list" aria-label="Repositories" data-i18n-attrs='{"aria-label":"tests.repositories_1e32af"}'></nav><button class="btn" type="button" id="test-run-open"><span data-i18n="tests.run_tests_3f6d6b">Run tests</span></button><span id="test-live-status" role="status"></span></aside><section id="test-runs-collection" tabindex="-1"></section></section>`;
    paintNavigation(); paintRows(); bindSettings();
    const linkedRun = new URLSearchParams(location.hash.split('?')[1] || '').get('run');
    if (linkedRun) {
      const row = query('[data-test-run-id="' + CSS.escape(linkedRun) + '"]');
      if (row) { row.querySelector('details').open = true; row.querySelector('[data-test-logs]').focus({ preventScroll: true }); row.scrollIntoView({ block: 'center' }); }
      else window.DevCoordinatorI18n.text(query('#test-live-status'), "tests.the_linked_run_is_no_longer_available_in_retaine_a39a19");
    }
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
