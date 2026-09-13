'use strict';

window.DevCoordinatorArtifacts = (() => {
  const CHUNK_BYTES = 184320;
  const TEXT_BYTES = 1048576;
  const FILE_BYTES = 32 * TEXT_BYTES;
  const imageTypes = { png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif', webp: 'image/webp', bmp: 'image/bmp' };
  const textTypes = new Set(['txt', 'log', 'json', 'jsonl', 'xml', 'xsd', 'csv', 'tsv', 'md', 'yaml', 'yml', 'toml', 'html', 'svg', 'lock', 'trx']);

  function bundles(run) {
    return (run.checks || []).flatMap((check) => (check.retained_artifacts || []).map((artifact) => ({ ...artifact, check: check.name })));
  }

  function open(run, opener, { api, esc, bytes, signal, highlight, initialFile }) {
    document.getElementById('test-artifacts-dialog')?.dispatchEvent(new Event('cancel', { cancelable: true }));
    let choices = bundles(run);
    let activeRun = { ...run };
    const retainedRuns = new Map([[run.run_id, { run: activeRun }]]);
    const dialog = document.createElement('dialog');
    dialog.id = 'test-artifacts-dialog';
    dialog.setAttribute('aria-labelledby', 'artifact-title');
    dialog.innerHTML = `<div class="dialog-head"><div><h2 id="artifact-title">Evidence files</h2><p class="muted artifact-run-facts">${esc(run.display_name)} · ${esc(run.test)} · <time datetime="${esc(run.started_at)}">${esc(new Date(run.started_at).toLocaleString())}</time></p></div><button class="dialog-close" type="button" aria-label="Close evidence files">×</button></div>
      <div class="artifact-run-picker"><label class="f">Run<select aria-label="Run" disabled><option value="${esc(run.run_id)}">Latest run · ${esc(new Date(run.started_at).toLocaleString())}</option></select></label><div class="artifact-history-state" role="status"></div></div>
      <label class="f artifact-bundle"${choices.length < 2 ? ' hidden' : ''}>File collection<select aria-label="File collection">${choices.map((choice, index) => `<option value="${index}">${esc(choice.check)} · ${esc(choice.name)} · ${choice.files} files</option>`).join('')}</select></label>
      <div class="artifact-workspace"><nav class="artifact-navigation" aria-label="Evidence files"><div class="artifact-files"></div><div class="artifact-page-state" role="status"></div></nav><section class="artifact-preview" aria-label="Selected file"><header><h3 class="artifact-name" tabindex="-1"></h3><button class="btn" type="button" data-artifact-download disabled>Download file</button></header><div class="artifact-file-state" role="status"></div><div class="artifact-content"></div></section></div>`;
    document.body.appendChild(dialog);
    const presenter = window.DevCoordinatorArtifactContent;
    const query = (selector) => dialog.querySelector(selector);
    const fileList = query('.artifact-files');
    const pageState = query('.artifact-page-state');
    const content = query('.artifact-content');
    const fileState = query('.artifact-file-state');
    const download = query('[data-artifact-download]');
    let collection = null;
    let manifest = null;
    let nextOffset = null;
    let entries = [];
    let selection = null;
    let fileVersion = 0;
    let collectionVersion = 0;
    let closed = false;
    let imageUrl = null;
    let completeBlob = null;

    const clearPreview = () => {
      content.replaceChildren();
      if (imageUrl) URL.revokeObjectURL(imageUrl);
      imageUrl = null;
      completeBlob = null;
      download.disabled = true;
      fileState.textContent = '';
    };
    const close = () => {
      if (closed) return;
      closed = true;
      fileVersion += 1;
      collectionVersion += 1;
      clearPreview();
      signal?.removeEventListener('abort', close);
      dialog.close();
      dialog.remove();
      const target = opener?.isConnected ? opener : document.querySelector(`[data-test-artifacts][data-run-id="${CSS.escape(run.run_id)}"]`);
      target?.focus();
    };
    const errorState = (target, error, retry, label = 'Try again') => {
      target.replaceChildren();
      const message = document.createElement('p');
      message.textContent = error.message;
      const button = document.createElement('button');
      button.className = 'btn'; button.type = 'button'; button.textContent = label;
      button.addEventListener('click', retry);
      target.append(message, button);
    };
    const readFile = async (entry, limit, version) => {
      const parts = [];
      let offset = 0;
      do {
        if (closed || version !== fileVersion) return null;
        const chunk = await api('test.artifact.file', {
          path: run.worktree_path, run_id: activeRun.run_id, check: collection.check, artifact: collection.name,
          manifest_sha256: manifest, file: entry.path, offset, max_bytes: Math.max(1, Math.min(CHUNK_BYTES, limit - offset)),
        }, false);
        if (closed || version !== fileVersion) return null;
        const block = Uint8Array.from(atob(chunk.base64), (character) => character.charCodeAt(0));
        const end = offset + block.length;
        if (chunk.run_id !== activeRun.run_id || chunk.check !== collection.check || chunk.artifact !== collection.name
          || chunk.file !== entry.path || chunk.sha256 !== entry.sha256 || chunk.total_bytes !== entry.size
          || chunk.offset !== offset || chunk.bytes !== block.length || end > Math.min(entry.size, limit)
          || (block.length === 0 && offset < entry.size) || chunk.next_offset !== (end < entry.size ? end : null)) {
          throw new Error('The retained file changed or its response was incomplete.');
        }
        parts.push(block);
        offset = end;
      } while (offset < Math.min(entry.size, limit));
      return new Blob(parts, { type: 'application/octet-stream' });
    };
    const selectFile = async (entry, focus = true) => {
      selection = entry;
      const version = ++fileVersion;
      clearPreview();
      query('.artifact-name').textContent = entry.displayLabel;
      if (focus) query('.artifact-name').focus({ preventScroll: true });
      for (const button of fileList.querySelectorAll('button')) button.setAttribute('aria-current', String(button.dataset.artifactFile === entry.path));
      const extension = entry.path.split('.').pop().toLowerCase();
      const imageType = imageTypes[extension];
      if (entry.size > FILE_BYTES) {
        fileState.textContent = 'This file exceeds the 32 MiB browser limit. It remains available through the retained-file command line tools.';
        return;
      }
      if (!imageType && !textTypes.has(extension)) {
        fileState.textContent = 'No browser preview for this file type.';
        download.disabled = false;
        return;
      }
      fileState.textContent = 'Loading file…';
      try {
        const blob = await readFile(entry, imageType ? FILE_BYTES : TEXT_BYTES, version);
        if (!blob) return;
        completeBlob = blob.size === entry.size ? blob : null;
        fileState.textContent = blob.size < entry.size ? 'Preview limited to 1 MiB. Download the file for the complete content.' : bytes(entry.size);
        if (imageType) {
          const image = document.createElement('img');
          image.alt = entry.displayLabel;
          imageUrl = URL.createObjectURL(new Blob([blob], { type: imageType }));
          image.src = imageUrl;
          await image.decode();
          if (closed || version !== fileVersion) return;
          content.append(image);
        } else {
          const text = await blob.text();
          if (closed || version !== fileVersion) return;
          content.append(presenter.render(text, entry.path, { truncated: blob.size < entry.size, highlight }));
        }
        download.disabled = false;
      } catch (error) {
        if (closed || version !== fileVersion) return;
        clearPreview();
        errorState(fileState, error, () => selectFile(entry));
      }
    };
    const loadPage = async (offset, version) => {
      pageState.textContent = 'Loading files…';
      try {
        const page = await api('test.artifact.catalog', {
          path: run.worktree_path, run_id: activeRun.run_id, check: collection.check, artifact: collection.name,
          offset, limit: 100, ...(manifest ? { manifest_sha256: manifest } : {}),
        }, false);
        if (closed || version !== collectionVersion) return;
        if (page.run_id !== activeRun.run_id || page.check !== collection.check || page.artifact?.name !== collection.name
          || page.artifact.sha256 !== collection.sha256 || (manifest && page.manifest_sha256 !== manifest)
          || page.entries.length > 100 || offset + page.entries.length > 4096
          || (page.next_offset != null && (page.next_offset !== offset + page.entries.length || page.next_offset <= offset))) {
          throw new Error('The retained file catalogue changed or was incomplete.');
        }
        manifest = page.manifest_sha256;
        nextOffset = page.next_offset;
        entries.push(...page.entries);
        const groups = new Map();
        for (const entry of entries) {
          const description = presenter.describe(entry.path);
          entry.description = description;
          const key = description.title + ':' + description.kind;
          if (!groups.has(key)) groups.set(key, []);
          groups.get(key).push(entry);
        }
        for (const group of groups.values()) group.forEach((entry, index) => {
          entry.displayLabel = entry.description.title + (group.length > 1 ? ' · ' + (index + 1) : '');
        });
        for (const entry of page.entries) {
          const button = document.createElement('button');
          button.type = 'button'; button.dataset.artifactFile = entry.path;
          const title = document.createElement('span');
          title.className = 'artifact-file-label';
          const kind = document.createElement('span');
          kind.className = 'artifact-file-kind'; kind.textContent = entry.description.kind;
          button.append(title, kind);
          button.addEventListener('click', () => selectFile(entry));
          fileList.append(button);
        }
        for (const button of fileList.querySelectorAll('[data-artifact-file]')) {
          const entry = entries.find((item) => item.path === button.dataset.artifactFile);
          button.querySelector('.artifact-file-label').textContent = entry.displayLabel;
        }
        if (selection) query('.artifact-name').textContent = selection.displayLabel;
        pageState.replaceChildren();
        if (nextOffset != null) {
          const more = document.createElement('button');
          more.className = 'btn'; more.type = 'button'; more.textContent = 'More files';
          more.addEventListener('click', () => loadPage(nextOffset, version));
          pageState.append(more);
        }
        if (!entries.length) pageState.textContent = 'No retained files in this collection.';
        if (!selection && initialFile) {
          const requested = entries.find((entry) => entry.path === initialFile.path);
          if (requested) { initialFile = null; await selectFile(requested, false); }
          else if (nextOffset != null) await loadPage(nextOffset, version);
          else { initialFile = null; fileState.textContent = 'The selected file is no longer available.'; }
        } else if (!selection && entries.length) await selectFile(entries[0], false);
      } catch (error) {
        if (closed || version !== collectionVersion) return;
        errorState(pageState, error, () => loadPage(offset, version));
      }
    };
    const loadCollection = () => {
      collection = choices[Number(query('.artifact-bundle select').value)];
      entries = []; selection = null; manifest = null; nextOffset = null;
      collectionVersion += 1; fileVersion += 1;
      clearPreview(); fileList.replaceChildren(); query('.artifact-name').textContent = '';
      if (collection) loadPage(0, collectionVersion);
      else pageState.textContent = 'No retained files are available for this run.';
    };
    const updateFacts = () => {
      query('.artifact-run-facts').textContent = `${run.display_name} · ${activeRun.test} · ${new Date(activeRun.started_at).toLocaleString()}`;
    };
    const renderCollections = () => {
      query('.artifact-bundle').hidden = choices.length < 2;
      query('.artifact-bundle select').innerHTML = choices.map((choice, index) => `<option value="${index}">${esc(choice.check)} · ${esc(choice.name)} · ${choice.files} files</option>`).join('');
      updateFacts();
      loadCollection();
    };
    const selectRun = async () => {
      const record = retainedRuns.get(query('.artifact-run-picker select').value);
      if (!record) return;
      activeRun = record.run;
      choices = [];
      renderCollections();
      if (activeRun.run_id === run.run_id && !run.checks_truncated) {
        choices = bundles(run);
        renderCollections();
        return;
      }
      pageState.textContent = 'Finding retained files…';
      const version = collectionVersion;
      try {
        const checks = new Set();
        const cursors = new Set();
        let cursor = null;
        do {
          const catalog = await api('test.log.catalog', { path: run.worktree_path, run_id: activeRun.run_id, limit: 100, ...(cursor ? { cursor } : {}) }, false);
          if (closed || version !== collectionVersion) return;
          for (const entry of catalog.entries) {
            const reference = entry.log_ref;
            if (reference.run_id !== activeRun.run_id) throw new Error('The selected run catalogue changed.');
            if (reference.check && reference.phase === 'check') checks.add(reference.check);
          }
          cursor = catalog.next_cursor;
          if (cursor && (cursors.has(cursor) || cursors.size >= 4096)) throw new Error('The selected run catalogue could not be fully read.');
          if (cursor) cursors.add(cursor);
        } while (cursor);
        const catalogs = await Promise.all([...checks].map(async (check) => {
          try {
            const result = await api('test.artifact.catalog', { path: run.worktree_path, run_id: record.run.run_id, check, limit: 100 }, false);
            if (result.run_id !== record.run.run_id || result.check !== check) throw new Error('The retained file catalogue changed or was incomplete.');
            return result;
          } catch (error) {
            if (error.code === 'test_artifact_not_found' || error.code === 'test_artifact_expired') return null;
            throw error;
          }
        }));
        if (closed || version !== collectionVersion) return;
        choices = catalogs.filter(Boolean).flatMap((catalog) => catalog.artifacts.map((artifact) => ({ ...artifact, check: catalog.check })));
        renderCollections();
      } catch (error) {
        if (closed || version !== collectionVersion) return;
        errorState(pageState, error, selectRun);
      }
    };
    const loadHistory = async () => {
      const target = query('.artifact-history-state');
      target.textContent = 'Loading runs…';
      const seen = new Set();
      const records = new Map([[run.run_id, { run }]]);
      let before = null;
      try {
        do {
          const history = await api('test.history', { path: run.worktree_path, limit: 50, ...(before ? { before } : {}) }, false);
          if (closed) return;
          if (!Array.isArray(history.runs) || history.runs.length > 50) throw new Error('Run history could not be fully read.');
          for (const entry of history.runs) records.set(entry.run_id, { run: entry });
          before = history.next_before;
          if (before && (seen.has(before) || seen.size >= 20)) throw new Error('Run history changed. Try loading it again.');
          if (before) seen.add(before);
        } while (before);
        retainedRuns.clear();
        for (const [identity, record] of records) retainedRuns.set(identity, record);
        const picker = query('.artifact-run-picker select');
        picker.innerHTML = [...retainedRuns.values()].sort((left, right) => right.run.started_at.localeCompare(left.run.started_at))
          .map((record) => `<option value="${esc(record.run.run_id)}">${esc(new Date(record.run.started_at).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', second: '2-digit' }))}</option>`).join('');
        picker.value = activeRun.run_id;
        picker.disabled = retainedRuns.size < 2;
        target.textContent = '';
        query('.artifact-run-picker').hidden = retainedRuns.size < 2;
      } catch (error) {
        if (closed) return;
        errorState(target, error, loadHistory, 'Retry run history');
      }
    };
    query('.artifact-run-picker select').addEventListener('change', selectRun);
    download.addEventListener('click', async () => {
      if (!selection || download.disabled) return;
      const entry = selection;
      const version = fileVersion;
      download.disabled = true;
      try {
        const blob = completeBlob || await readFile(entry, FILE_BYTES, version);
        if (!blob || closed || version !== fileVersion) return;
        const url = URL.createObjectURL(blob);
        const link = document.createElement('a');
        link.href = url; link.download = entry.displayLabel.replaceAll('/', ' - ') + (entry.description.extension ? '.' + entry.description.extension : '');
        document.body.appendChild(link); link.click(); link.remove();
        setTimeout(() => URL.revokeObjectURL(url), 1000);
        download.disabled = false;
      } catch (error) {
        if (closed || version !== fileVersion) return;
        errorState(fileState, error, () => { download.disabled = false; download.click(); });
      }
    });
    query('.dialog-close').addEventListener('click', close);
    query('.artifact-bundle select').addEventListener('change', loadCollection);
    dialog.addEventListener('cancel', (event) => { event.preventDefault(); close(); });
    signal?.addEventListener('abort', close, { once: true });
    dialog.showModal();
    if (initialFile) {
      const index = choices.findIndex((choice) => choice.check === initialFile.check && choice.name === initialFile.artifact);
      if (index >= 0) query('.artifact-bundle select').value = String(index);
    }
    if (run.checks_truncated) selectRun();
    else loadCollection();
    loadHistory();
  }

  async function previews(run, { api, signal, urls }) {
    const images = [];
    for (const collection of bundles(run)) {
      let offset = 0;
      let manifest;
      do {
        if (signal.aborted) throw new Error('Screenshot loading cancelled.');
        const catalog = await api('test.artifact.catalog', { path: run.worktree_path, run_id: run.run_id, check: collection.check, artifact: collection.name, offset, limit: 100, ...(manifest ? { manifest_sha256: manifest } : {}) });
        if (catalog.run_id !== run.run_id || catalog.check !== collection.check || catalog.artifact?.name !== collection.name || catalog.artifact.sha256 !== collection.sha256 || (manifest && catalog.manifest_sha256 !== manifest) || catalog.entries.length > 100 || (catalog.next_offset != null && catalog.next_offset !== offset + catalog.entries.length)) throw new Error('The retained file catalogue changed.');
        manifest = catalog.manifest_sha256;
        for (const entry of catalog.entries) {
          const mime = imageTypes[entry.path.split('.').pop().toLowerCase()];
          if (!mime || entry.size > FILE_BYTES) continue;
          const load = async () => {
            const parts = [];
            let position = 0;
            do {
              if (signal.aborted) throw new Error('Screenshot loading cancelled.');
              const chunk = await api('test.artifact.file', { path: run.worktree_path, run_id: run.run_id, check: collection.check, artifact: collection.name, manifest_sha256: manifest, file: entry.path, offset: position, max_bytes: CHUNK_BYTES });
              const block = Uint8Array.from(atob(chunk.base64), (character) => character.charCodeAt(0));
              const end = position + block.length;
              if (chunk.run_id !== run.run_id || chunk.check !== collection.check || chunk.artifact !== collection.name || chunk.file !== entry.path || chunk.sha256 !== entry.sha256 || chunk.total_bytes !== entry.size || chunk.offset !== position || chunk.bytes !== block.length || !block.length || end > entry.size || chunk.next_offset !== (end < entry.size ? end : null)) throw new Error('The retained image changed.');
              parts.push(block); position = end;
            } while (position < entry.size);
            if (signal.aborted) throw new Error('Screenshot loading cancelled.');
            const url = URL.createObjectURL(new Blob(parts, { type: mime }));
            urls.add(url);
            return url;
          };
          images.push({ native: true, load, label: window.DevCoordinatorArtifactContent.describe(entry.path).title, path: entry.path, check: collection.check, artifact: collection.name });
        }
        if (catalog.next_offset != null && catalog.next_offset <= offset) throw new Error('The retained file catalogue was incomplete.');
        offset = catalog.next_offset;
      } while (offset != null && offset < 4096);
    }
    return { run, images, count: images.length };
  }

  return { bundles, open, previews };
})();
