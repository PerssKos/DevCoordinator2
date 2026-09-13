'use strict';

window.DevCoordinatorWorkspace = (() => {
  const repositoryViews = new Set(['plan', 'progress', 'usage', 'deployments', 'tests', 'decisions', 'glossary']);
  const workViews = new Set(['plan', 'progress', 'usage']);
  const icons = {
    folder: ['Folder', 'M3 7V5h6l3 3h9v12H3Z'],
    code: ['Code', 'm8 7-5 5 5 5m8-10 5 5-5 5m-3-13-2 16'],
    'app-window': ['App', 'M3 4h18v16H3Zm0 5h18M6 6.5h.01M9 6.5h.01'],
    world: ['Web', 'M3 12a9 9 0 1 0 18 0 9 9 0 1 0-18 0m0 0h18M12 3c-5 5-5 13 0 18 5-5 5-13 0-18'],
    rocket: ['Launch', 'm13 4 7-1-1 7-8 8-5-5Zm-6 7-4 1v5l4-1m6 1-1 4h5l1-4M5 19l-2 2m12-13h.01'],
    database: ['Data', 'M4 6c0-4 16-4 16 0s-16 4-16 0m0 0v12c0 4 16 4 16 0V6M4 12c0 4 16 4 16 0'],
    'device-desktop': ['Desktop', 'M3 4h18v13H3Zm5 17h8m-4-4v4'],
    'device-mobile': ['Mobile', 'M7 3h10v18H7Zm4 15h2'],
    tools: ['Tools', 'm3 21 10-10m-3-4a6 6 0 0 0 7 7l4-4-5 1-3-3 1-5-4 4M3 3l4 1 1 3-2 2-3-2Zm4 4 14 14'],
    flask: ['Research', 'M9 3h6m-5 0v7L4 20h16l-6-10V3M7 15h10'],
    palette: ['Design', 'M12 3a9 9 0 1 0 0 18h2a2 2 0 0 0 0-4h-1a2 2 0 0 1 0-4h4c6 0 4-10-5-10M7 8h.01M12 6h.01M17 8h.01M5 13h.01'],
    star: ['Favorite', 'm12 3 3 6 7 1-5 5 1 7-6-3-6 3 1-7-5-5 7-1Z'],
    plane: ['Flight', 'm12 3 2 7 7 4v2l-7-2v5l2 2-4-1-4 1 2-2v-5l-7 2v-2l7-4Z'],
    book: ['Docs', 'M12 5v16M3 3c4 0 7 1 9 2 2-1 5-2 9-2v16c-4 0-7 1-9 2-2-1-5-2-9-2Z'],
    'chart-bar': ['Analytics', 'M3 21h18M5 21V11h3v10m3 0V3h3v18m3 0v-7h3v7'],
    shield: ['Security', 'm12 3 9 4v5c0 5-6 8-9 10-3-2-9-5-9-10V7Zm-4 9 3 3 5-6'],
  };
  const repositoryIcon = (name) => `<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="${(icons[name] || icons.folder)[1]}"/></svg>`;

  function catalogue(repositories, runs, deployments) {
    const records = new Map();
    const add = (row) => {
      if (!row.repository_id) return;
      const previous = records.get(row.repository_id);
      records.set(row.repository_id, {
        ...previous, ...row,
        display_name: previous?.display_name || row.display_name || 'Repository',
        root_path: previous?.root_path || row.root_path,
        repository_source: previous?.repository_source || row.repository_source,
        paths: [...new Set([...(previous?.paths || []), row.root_path, row.worktree_path].filter(Boolean))],
      });
    };
    repositories.forEach(add);
    runs.forEach(add);
    deployments.forEach((deployment) => add({ repository_id: deployment.repository_id, display_name: deployment.repository_name }));
    const groups = new Map();
    for (const record of records.values()) {
      const key = record.repository_source?.key || record.repository_id;
      if (!groups.has(key)) groups.set(key, { key, name: record.repository_source?.name || record.display_name, records: [] });
      groups.get(key).records.push(record);
    }
    for (const group of groups.values()) {
      group.records.sort((left, right) => Number(right.display_name === group.name) - Number(left.display_name === group.name)
        || left.display_name.localeCompare(right.display_name) || left.repository_id.localeCompare(right.repository_id));
      group.repositoryId = group.records[0].repository_id;
      group.paths = [...new Set(group.records.flatMap((record) => record.paths))];
      group.rootPath = group.records[0].root_path || group.paths[0] || '';
      group.defaultName = group.name;
      group.presentation = group.records[0].presentation;
      group.name = group.presentation?.display_name || group.defaultName;
      group.icon = Object.hasOwn(icons, group.presentation?.icon) ? group.presentation.icon : 'folder';
    }
    return [...groups.values()].sort((left, right) => left.name.localeCompare(right.name) || left.key.localeCompare(right.key));
  }

  function href(view, repositoryId) {
    return ['tests', 'deployments'].includes(view)
      ? `#/${view}?repository=${encodeURIComponent(repositoryId)}`
      : `#/${view}/${encodeURIComponent(repositoryId)}`;
  }

  function create({ api, esc, identity }) {
    const shell = document.querySelector('#repository-workspace');
    const sidebar = document.querySelector('#repository-sidebar');
    const navigation = document.querySelector('#repository-list');
    const heading = document.querySelector('#workspace-heading');
    const aspects = document.querySelector('#workspace-aspects');
    const workNavigation = document.querySelector('#workspace-work-views');
    const toggle = document.querySelector('#repository-toggle');
    const overlay = document.querySelector('#repository-overlay');
    const refreshButton = document.querySelector('#repository-refresh');
    const search = document.querySelector('#repository-search');
    const editPresentation = document.querySelector('#repository-presentation');
    const resize = document.querySelector('#repository-resize');
    const narrow = window.matchMedia('(max-width: 760px)');
    let sidebarWidth = 280;
    let sidebarCollapsed = false;
    try {
      const storedWidth = Number(localStorage.getItem('dc2-repository-width'));
      if (storedWidth >= 240 && storedWidth <= 480) sidebarWidth = storedWidth;
      sidebarCollapsed = localStorage.getItem('dc2-repository-collapsed') === 'true';
    } catch {}
    let data;
    let testsLoaded = false;
    let retainedEvidence;
    let groups = [];
    let selectedId = '';
    let currentView = 'plan';
    let active = false;
    let loading;
    let searchText = '';
    let remembered = '';
    let drawerBackground = null;
    try { remembered = sessionStorage.getItem('dc2-workspace-repository') || ''; } catch {}

    const groupFor = (repositoryId) => groups.find((group) => group.records.some((record) => record.repository_id === repositoryId));
    const current = () => groupFor(selectedId);
    const canOperate = () => identity()?.administrator || Object.values(identity()?.grants || {}).some((role) => ['operator', 'administrator'].includes(role));

    function paintLayout() {
      const maximum = Math.max(240, Math.min(480, window.innerWidth - 400));
      const width = Math.min(sidebarWidth, maximum);
      shell.style.setProperty('--repository-sidebar-width', `${width}px`);
      shell.classList.toggle('repository-collapsed', !narrow.matches && sidebarCollapsed);
      const content = document.querySelector('.workspace-content');
      content.classList.toggle('workspace-narrow', content.clientWidth <= 800);
      resize.setAttribute('aria-valuemax', String(maximum));
      resize.setAttribute('aria-valuenow', String(width));
      const expanded = narrow.matches ? shell.classList.contains('repository-drawer-open') : !sidebarCollapsed;
      const drawerOpen = narrow.matches && expanded;
      if (drawerOpen && !drawerBackground) {
        drawerBackground = [document.querySelector('.top'), document.querySelector('#banner'), content].filter(Boolean).map(element => ({ element, inert: element.inert, hidden: element.getAttribute('aria-hidden') }));
        for (const { element } of drawerBackground) { element.inert = true; element.setAttribute('aria-hidden', 'true'); }
        sidebar.setAttribute('role', 'dialog');
        sidebar.setAttribute('aria-modal', 'true');
        sidebar.dataset.uiContextualOverlay = 'Temporary repository navigation drawer';
      } else if (!drawerOpen && drawerBackground) {
        for (const { element, inert, hidden } of drawerBackground) {
          element.inert = inert;
          if (hidden === null) element.removeAttribute('aria-hidden');
          else element.setAttribute('aria-hidden', hidden);
        }
        drawerBackground = null;
        sidebar.removeAttribute('role');
        sidebar.removeAttribute('aria-modal');
        delete sidebar.dataset.uiContextualOverlay;
      }
      toggle.setAttribute('aria-expanded', String(expanded));
      toggle.setAttribute('aria-label', expanded ? 'Hide repositories' : 'Show repositories');
    }

    function setSidebarCollapsed(collapsed) {
      sidebarCollapsed = collapsed;
      try { localStorage.setItem('dc2-repository-collapsed', String(collapsed)); } catch {}
      paintLayout();
    }

    function closeDrawer(restoreFocus = false) {
      shell.classList.remove('repository-drawer-open');
      overlay.hidden = true;
      paintLayout();
      if (restoreFocus) toggle.focus();
    }

    function paintNavigation() {
      const visible = groups.filter((group) => [group.name, group.defaultName || '', ...group.paths].some((value) => value.toLowerCase().includes(searchText.toLowerCase())));
      navigation.innerHTML = visible.length ? visible.map((group) => {
        const selected = group === current();
        const title = [group.name, group.rootPath].filter(Boolean).join('\n');
        return `<a class="workspace-repository" title="${esc(title)}" data-repository-icon="${group.icon || 'folder'}" href="${href(currentView, selected ? selectedId : group.repositoryId)}"${selected ? ' aria-current="page"' : ''}>${repositoryIcon(group.icon)}<span><span class="workspace-repository-name">${esc(group.name)}</span>${group.rootPath ? `<small>${esc(group.rootPath)}</small>` : ''}</span></a>`;
      }).join('') : `<p class="muted">${searchText ? 'No matching repositories.' : 'No repositories available.'}</p>`;
      const selected = current();
      const checkouts = document.querySelector('#workspace-checkouts');
      checkouts.hidden = !selected?.paths.length;
      checkouts.innerHTML = selected ? `<summary>Checkouts${selected.paths.length > 1 ? ` <span>${selected.paths.length}</span>` : ''}</summary><div>${selected.records.map((record) => `<div class="workspace-checkout">${record.paths.map((path) => `<span>${esc(path)}</span>`).join('')}${selected.records.length > 1 ? `<a href="${href(currentView, record.repository_id)}"${selectedId === record.repository_id ? ' aria-current="page"' : ''}>${selectedId === record.repository_id ? 'Selected' : 'Use this checkout’s plan & records'}</a>` : ''}</div>`).join('')}</div>` : '';
    }

    function paint() {
      shell.classList.toggle('repository-scoped', active);
      sidebar.hidden = !active;
      document.querySelector('#workspace-context').hidden = !active;
      closeDrawer();
      if (!active) { workNavigation.hidden = true; return; }
      const selected = current();
      heading.innerHTML = selected ? `${repositoryIcon(selected.icon)}<span>${esc(selected.name)}</span>` : 'Repositories';
      heading.title = selected?.name || 'Repositories';
      const root = document.querySelector('#workspace-root');
      root.hidden = !selected?.rootPath;
      root.textContent = selected?.rootPath ? `Root: ${selected.rootPath}` : '';
      editPresentation.hidden = !selected || !identity()?.administrator;
      toggle.title = selected ? `Current repository: ${selected.name}` : 'Repositories';
      const record = selected?.records.find((item) => item.repository_id === selectedId);
      const scope = document.querySelector('#workspace-record-scope');
      scope.hidden = !record || selectedId === selected?.repositoryId || ['tests', 'deployments'].includes(currentView);
      const checkoutPath = record?.paths[0] || record?.display_name || '';
      scope.textContent = scope.hidden ? '' : selected.rootPath && checkoutPath.startsWith(`${selected.rootPath}/`)
        ? `./${checkoutPath.slice(selected.rootPath.length + 1)}` : checkoutPath;
      scope.title = scope.hidden ? '' : checkoutPath;
      const tabs = [['plan', 'Plan & progress'], ['deployments', 'Deployments'], ['tests', 'Tests'], ['decisions', 'Decisions'], ['glossary', 'Glossary']];
      aspects.innerHTML = selectedId ? tabs.map(([view, label]) => `<a href="${href(view, selectedId)}"${(view === currentView || view === 'plan' && workViews.has(currentView)) ? ' aria-current="page"' : ''}>${label}</a>`).join('') : '';
      workNavigation.hidden = !selectedId || !workViews.has(currentView);
      workNavigation.innerHTML = selectedId && workViews.has(currentView) ? [['plan', 'Plan'], ...(canOperate() ? [['progress', 'Progress'], ['usage', 'Usage']] : [])].map(([view, label]) => `<a href="${href(view, selectedId)}"${view === currentView ? ' aria-current="page"' : ''}>${label}</a>`).join('') : '';
      paintNavigation();
    }

    function openPresentation() {
      const selected = current();
      if (!selected || !identity()?.administrator) return;
      const dialog = document.createElement('dialog');
      dialog.id = 'repository-presentation-dialog';
      dialog.className = 'repository-presentation-dialog';
      dialog.setAttribute('aria-labelledby', 'repository-presentation-title');
      dialog.innerHTML = `<form><h2 id="repository-presentation-title">Repository appearance</h2><label class="f">Name in Console<input name="display_name" maxlength="80" required value="${esc(selected.name)}" placeholder="${esc(selected.defaultName)}"></label><fieldset><legend>Icon</legend><div class="repository-icon-picker">${Object.entries(icons).map(([value, [label]]) => `<label><input type="radio" name="icon" value="${value}"${selected.icon === value ? ' checked' : ''}>${repositoryIcon(value)}<span>${label}</span></label>`).join('')}</div></fieldset><p class="muted">The repository and its checkout paths stay unchanged.</p><p role="alert" hidden></p><div class="actions"><button class="btn btn-small" type="button" data-presentation-reset>Use defaults</button><button class="btn" type="button" data-presentation-cancel>Cancel</button><button class="btn btn-primary" type="submit">Save</button></div></form>`;
      document.body.appendChild(dialog);
      const form = dialog.querySelector('form');
      const close = () => dialog.close();
      dialog.addEventListener('close', () => { dialog.remove(); editPresentation.focus(); });
      dialog.querySelector('[data-presentation-cancel]').addEventListener('click', close);
      dialog.querySelector('[data-presentation-reset]').addEventListener('click', () => {
        form.elements.display_name.value = selected.defaultName;
        form.elements.icon.value = 'folder';
      });
      form.addEventListener('submit', async (event) => {
        event.preventDefault();
        const displayName = form.elements.display_name.value.trim();
        const name = form.elements.display_name;
        name.setCustomValidity(!displayName || /[\u0000-\u001f\u007f-\u009f]/.test(displayName) ? 'Enter a repository name without control characters.' : '');
        if (!form.reportValidity()) return;
        const values = { repository_id: selected.repositoryId, display_name: displayName === selected.defaultName ? null : displayName,
          icon: form.elements.icon.value === 'folder' ? null : form.elements.icon.value };
        for (const control of form.elements) control.disabled = true;
        try {
          const result = await api('repository.presentation.update', values, false);
          data.repositories = data.repositories.map((record) => record.repository_id === selected.repositoryId ? { ...record, presentation: result } : record);
          groups = catalogue(data.repositories, data.runs, data.deployments);
          paint(); close();
        } catch (error) {
          const message = dialog.querySelector('[role=alert]');
          message.textContent = error.message || 'Could not save repository appearance. Try again.';
          message.hidden = false;
        } finally { for (const control of form.elements) control.disabled = false; }
      });
      form.elements.display_name.addEventListener('input', () => form.elements.display_name.setCustomValidity(''));
      dialog.showModal(); form.elements.display_name.focus(); form.elements.display_name.select();
    }

    editPresentation.addEventListener('click', openPresentation);

    async function load(signal) {
      if (data && (currentView !== 'tests' || testsLoaded)) return;
      if (loading?.signal === signal) return loading.promise;
      const read = async (operation) => {
        try { return { value: await api(operation, operation === 'usage.repositories' ? { range: '24h' } : {}) }; }
        catch (error) { if (signal.aborted || error.code === 'stale' || error.code === 'unauthenticated') throw error; return { error }; }
      };
      const includeTests = currentView === 'tests';
      const promise = Promise.all([read('plan.overview'), includeTests ? read('test.list') : { value: { runs: [] } }, read('deployment.list'), ...(canOperate() ? [read('usage.repositories'), read('progress.repositories')] : [])]).then(async ([plans, tests, deploymentList, usage, progress]) => {
        if (plans.error && !includeTests) tests = await read('test.list');
        if (signal.aborted) return;
        if ([plans, tests, deploymentList, usage, progress].filter(Boolean).every((result) => result.error)) throw plans.error;
        data = { repositories: [...(plans.value?.repositories || []), ...(usage?.value?.repositories || []), ...(progress?.value?.repositories || [])], runs: tests.value?.runs || [], deployments: deploymentList.value?.deployments || [] };
        testsLoaded = includeTests || !!plans.error;
        groups = catalogue(data.repositories, data.runs, data.deployments);
        const unavailable = [['Plan', plans], ['Tests', tests], ['Deployments', deploymentList]].filter(([, result]) => result.error && result.error.code !== 'permission_denied');
        document.querySelector('#repository-status').textContent = unavailable.length ? `${unavailable.map(([label]) => label).join(', ')} repositories unavailable. Refresh to retry.` : '';
      });
      loading = { signal, promise };
      return promise;
    }

    async function resolve(signal) {
      retainedEvidence = null;
      const [pathname, queryString = ''] = (location.hash || '#/plan').split('?');
      const [, view = 'plan', argument = ''] = pathname.split('/');
      currentView = view;
      active = repositoryViews.has(view) && !(view === 'glossary' && (!argument || argument === 'shared'));
      if (!active) { paint(); return { view, arg: argument }; }
      shell.classList.add('repository-scoped');
      sidebar.hidden = false;
      document.querySelector('#workspace-context').hidden = false;
      if (!data) navigation.innerHTML = '<p class="muted">Loading repositories…</p>';
      await load(signal);
      if (signal.aborted) return null;
      const query = new URLSearchParams(queryString);
      const matchesRun = run => (run.run_id === argument || run.earlier_visual_evidence?.run_id === argument) && (!query.get('worktree') || run.worktree_id === query.get('worktree'));
      if (view === 'tests' && argument && !data.runs.some(matchesRun)) {
        retainedEvidence = await api('test.evidence.lookup', { run_id: argument, image_id: query.get('image') || undefined, worktree_id: query.get('worktree') || undefined });
        if (signal.aborted) return null;
        data.runs.push({ ...retainedEvidence.context, isEarlierEvidence: true });
        groups = catalogue(data.repositories, data.runs, data.deployments);
      }
      let requested = ['plan', 'progress', 'usage', 'decisions', 'glossary'].includes(view) ? argument : query.get('repository');
      if (view === 'tests' && argument) requested = data.runs.find(matchesRun)?.repository_id || requested;
      if (view === 'deployments' && argument) requested = data.deployments.find((deployment) => deployment.deployment_id === argument)?.repository_id || requested;
      if (['tests', 'deployments'].includes(view) && argument && !requested) {
        active = false; selectedId = ''; paint();
        return { view, arg: `${argument}${view === 'tests' && queryString ? `?${queryString}` : ''}` };
      }
      selectedId = requested || (groupFor(remembered) ? remembered : groups[0]?.repositoryId) || '';
      if (requested && !groupFor(requested)) {
        const unmatchedDetail = ['tests', 'deployments'].includes(view) && argument;
        if (unmatchedDetail) { active = false; selectedId = ''; }
        else {
          groups.push({ key: requested, name: 'Repository unavailable', repositoryId: requested, paths: [], records: [{ repository_id: requested, display_name: 'Repository unavailable', paths: [] }] });
        }
      }
      if (selectedId) {
        remembered = selectedId;
        try { sessionStorage.setItem('dc2-workspace-repository', selectedId); } catch {}
        if (!argument && !query.has('repository')) {
          const target = href(view, selectedId);
          const separator = target.includes('?') ? '&' : '?';
          window.history.replaceState(null, '', `${target}${queryString ? `${separator}${queryString}` : ''}`);
        }
      }
      paint();
      return { view, arg: view === 'tests' ? argument && `${argument}${queryString ? `?${queryString}` : ''}` : view === 'deployments' ? argument : argument || selectedId, empty: !selectedId && !argument, settings: query.get('settings') };
    }

    toggle.addEventListener('click', () => {
      if (!narrow.matches) { setSidebarCollapsed(!sidebarCollapsed); return; }
      if (shell.classList.contains('repository-drawer-open')) { closeDrawer(true); return; }
      shell.classList.add('repository-drawer-open');
      toggle.setAttribute('aria-expanded', 'true');
      overlay.hidden = false;
      paintLayout();
      search.focus();
    });
    overlay.addEventListener('click', () => closeDrawer(true));
    document.querySelector('#repository-close').addEventListener('click', () => {
      if (!narrow.matches) setSidebarCollapsed(true);
      closeDrawer(true);
    });
    let resizing;
    const finishResize = (event) => {
      if (!resizing) return;
      if (event.type === 'pointercancel') sidebarWidth = resizing.width;
      else try { localStorage.setItem('dc2-repository-width', String(sidebarWidth)); } catch {}
      resizing = null;
      shell.classList.remove('repository-resizing');
      paintLayout();
    };
    resize.addEventListener('pointerdown', (event) => {
      if (event.button !== 0 || narrow.matches) return;
      resizing = { start: event.clientX, width: sidebar.getBoundingClientRect().width };
      resize.setPointerCapture(event.pointerId);
      shell.classList.add('repository-resizing');
      event.preventDefault();
    });
    resize.addEventListener('pointermove', (event) => {
      if (!resizing) return;
      sidebarWidth = Math.max(240, Math.min(Number(resize.getAttribute('aria-valuemax')), resizing.width + event.clientX - resizing.start));
      paintLayout();
    });
    resize.addEventListener('pointerup', finishResize);
    resize.addEventListener('pointercancel', finishResize);
    resize.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') { event.preventDefault(); setSidebarCollapsed(true); toggle.focus(); return; }
      const maximum = Number(resize.getAttribute('aria-valuemax'));
      const changes = { ArrowLeft: -10, ArrowRight: 10 };
      if (!Object.hasOwn(changes, event.key) && !['Home', 'End'].includes(event.key)) return;
      event.preventDefault();
      sidebarWidth = event.key === 'Home' ? 240 : event.key === 'End' ? maximum : Math.max(240, Math.min(maximum, sidebarWidth + changes[event.key]));
      try { localStorage.setItem('dc2-repository-width', String(sidebarWidth)); } catch {}
      paintLayout();
    });
    window.addEventListener('resize', paintLayout);
    narrow.addEventListener('change', () => closeDrawer());
    sidebar.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') { event.preventDefault(); closeDrawer(true); }
      if (event.key === 'Tab' && shell.classList.contains('repository-drawer-open')) {
        const controls = [...sidebar.querySelectorAll('a,button,input,summary')].filter((element) => element.getClientRects().length);
        const first = controls[0]; const last = controls.at(-1);
        if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
      }
    });
    sidebar.addEventListener('click', (event) => { if (event.target.closest('a')) closeDrawer(true); });
    search.addEventListener('input', () => { searchText = search.value; paintNavigation(); });
    refreshButton.addEventListener('click', async () => {
      refreshButton.disabled = true;
      data = null;
      try { await window.render(); } finally { refreshButton.disabled = false; }
    });
    return { resolve, href, current, get active() { return active; }, get repositoryId() { return selectedId; }, get retainedEvidence() { return retainedEvidence; },
      matches: (row) => current()?.records.some((record) => record.repository_id === row.repository_id) || false };
  }
  return { catalogue, href, create };
})();
