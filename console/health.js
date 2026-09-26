/* Health: explicit incident escalation, one responsive detail, real dispositions. */
(() => {
  'use strict';
  function create({ api, esc, bytes, pct, spark, chart, pageHeading, icon }) {
    const tr = (key, fallback, params = {}) => {
      try { return window.DevCoordinatorI18n?.t(`health.${key}`, params) || fallback; } catch { return fallback; }
    };
    const safe = (key, fallback, params = {}) => esc(tr(key, fallback, params));
    const stateLabel = value => { try { return window.DevCoordinatorI18n.t('common.status_' + String(value).replaceAll('-', '_')); } catch { return value; } };
    const s = { expanded: false, selected: null, view: 'attention', query: '', range: '24h', reports: null, reportError: null, repos: [], repoQuery: '', sort: 'cpu_percent', expandedRepos: new Set(), epoch: 0, incidentRead: 0 };
    const main = () => document.querySelector('#main');
    const phone = matchMedia('(max-width: 760px)');
    const ranges = { '24h': [1440, 288], '7d': [10080, 336], '30d': [43200, 360] };
    let summaryResult, repositoryError;
    const history = new Map();
    const date = (v) => v && !Number.isNaN(Date.parse(v))
      ? (window.DevCoordinatorI18n?.date ? window.DevCoordinatorI18n.date(v) : new Date(v).toLocaleString())
      : tr('unavailable','Unavailable');
    const age = (v) => {
      const minutes = Math.max(0, Math.floor((Date.now() - Date.parse(v)) / 60000));
      if (!Number.isFinite(minutes)) return tr('time_unavailable','Time unavailable');
      const formatter = window.DevCoordinatorI18n?.relative;
      if (formatter) {
        if (minutes < 60) return formatter(-minutes, 'minute');
        if (minutes < 1440) return formatter(-Math.floor(minutes / 60), 'hour');
        return formatter(-Math.floor(minutes / 1440), 'day');
      }
      return minutes < 60 ? `${minutes}m ago` : minutes < 1440 ? `${Math.floor(minutes / 60)}h ago` : `${Math.floor(minutes / 1440)}d ago`;
    };
    const badge = (label, kind = '') => `<span class="badge ${kind}">${esc(label)}</span>`;
    const fault = (message, retry) => `<div class="notice hi-error" role="alert"><span><strong>${safe('could_not_load','Could not load.')}</strong> ${esc(message)}</span><button type="button" class="btn" data-hi-retry="${retry}">${safe('retry','Retry')}</button></div>`;
    const heading = () => `<div class="hi-heading">${pageHeading(tr('health_558984','Health'), '#/health')}<div class="hi-heading-actions"><button class="btn" type="button" data-hi-refresh aria-label="${esc(tr('refresh_health','Refresh Health'))}">${icon('refresh')}</button><a class="btn" href="#/health/containers">${safe('view_containers_e26e60','View containers')}</a></div></div>`;

    function incidentDetail(i) {
      const dismissed = i.status === 'dismissed';
      const action = i.deployment_id
        ? `<a class="btn btn-primary" href="#/deployments/${encodeURIComponent(i.deployment_id)}">${safe('open_deployment','Open deployment')} ${icon('arrow-right')}</a>`
        : `<button class="btn btn-primary" type="button" data-hi-resources="${i.alert_key === 'host/disk' ? 'storage_bytes' : i.alert_key === 'host/memory' ? 'memory_bytes' : 'cpu_percent'}">${i.alert_key === 'host/disk' ? tr('review_storage','Review storage') : tr('view_resource_use','View resource use')} ${icon('arrow-right')}</button>`;
      const section = (symbol, title, text) => `<section class="hi-answer">${icon(symbol)}<div><h3>${title}</h3><p>${esc(text)}</p></div></section>`;
      return `<section class="hi-detail" id="hi-selected-detail" tabindex="-1" aria-label="${esc(tr('incident_explanation','Incident explanation'))}" data-ui-region="incident-detail">
        <header class="hi-detail-heading"><span>${esc(i.repository_name || tr('host_4a8231','Host'))}${i.deployment_name ? ` / ${esc(i.deployment_name)}` : ''}</span><h2>${esc(i.summary)}</h2><div>${badge(dismissed ? tr('dismissed','Dismissed') : i.severity === 'critical' ? tr('critical','Critical') : tr('warning','Warning'), dismissed ? '' : i.severity === 'critical' ? 'bad' : 'warn')}<time title="${esc(date(i.opened_at))}">${safe('detected','Detected')} ${esc(age(i.opened_at))}</time></div></header>
        ${section('info-circle', tr('what_happened','What happened'), i.what_happened)}
        ${section('user', tr('agent_response','Agent response'), i.agent_response || tr('response_history_unavailable','Response history is unavailable.'))}
        ${section('flag', tr('why_it_needs_you','Why it needs you'), i.escalation_reason || tr('escalation_reason_unavailable','Escalation reason is unavailable.'))}
        ${section('tool', tr('next_step','Next step'), i.next_step || tr('next_step_unavailable','A next step has not been recorded.'))}
        <div class="hi-incident-actions">${action}<button class="btn" type="button" data-hi-disposition="${dismissed ? 'escalated' : 'dismissed'}"${dismissed && !i.condition_active ? ' disabled data-i18n-attrs=\'{"title":"health.occurrence_recovered"}\'' : ''}>${dismissed ? tr('restore','Restore') : tr('dismiss','Dismiss')}</button></div>
        <div class="hi-action-result" role="status" aria-live="polite"></div>
        ${!i.condition_active ? `<p class="hi-recovered">${safe('condition_recovered','The observed condition has recovered.')}</p>` : ''}
        <details class="hi-technical"><summary>${i.deployment_id ? tr('details_logs','Details & logs') : tr('details','Details')}</summary><dl><div><dt>${safe('detected','Detected')}</dt><dd>${esc(date(i.opened_at))}</dd></div><div><dt>${safe('last_observed','Last observed')}</dt><dd>${esc(date(i.last_seen_at))}</dd></div><div><dt>${safe('response_recorded','Response recorded')}</dt><dd>${esc(date(i.updated_at))}</dd></div>${i.component ? `<div><dt>${safe('component','Component')}</dt><dd>${esc(i.component)}</dd></div>` : ''}</dl>${i.deployment_id ? `<a href="#/deployments/${encodeURIComponent(i.deployment_id)}">${safe('open_component_logs','Open component logs')}</a>` : ''}</details>
      </section>`;
    }

    function incidentsMarkup() {
      const r = s.reports;
      const count = r?.attention_count;
      return `<section class="hi-incidents" data-ui-region="health-incidents" aria-label="${esc(tr('reported_incidents_5c3cb2','Reported incidents'))}">
        <div class="hi-disclosure"><button type="button" id="hi-incidents-toggle" aria-expanded="${s.expanded}" aria-controls="hi-incident-panel">${icon(s.expanded ? 'chevron-up' : 'chevron-down')}<span class="hi-attention ${count ? 'active' : ''}" aria-hidden="true"></span><strong>${safe('reported_incidents_5c3cb2','Reported incidents')}</strong>${count != null ? `<span class="hi-count">${count}</span>` : ''}<span class="hi-expand-label">${s.expanded ? tr('collapse_be6eb1','Collapse') : tr('expand_07548c','Expand')}</span></button>${r?.dismissed_count ? `<button type="button" class="hi-dismissed-link" data-hi-dismissed>${safe('view_dismissed','View dismissed')} (${r.dismissed_count})</button>` : ''}</div>
        ${s.reportError ? fault(tr('incident_reports_unavailable','Incident reports unavailable.') + ' ' + s.reportError, 'incidents') : ''}
        <div id="hi-incident-panel"${s.expanded ? '' : ' hidden'}>
          <div class="hi-inbox-tools"><label><span class="sr-only">${safe('search_incidents','Search incidents')}</span><input type="search" data-hi-search placeholder="${esc(tr('search','Search'))}" value="${esc(s.query)}"></label><div role="group" aria-label="${esc(tr('incident_view','Incident view'))}"><button type="button" class="btn ${s.view === 'attention' ? 'active' : ''}" data-hi-view="attention" aria-pressed="${s.view === 'attention'}">${safe('active','Active')} (${r?.attention_count ?? '—'})</button><button type="button" class="btn ${s.view === 'dismissed' ? 'active' : ''}" data-hi-view="dismissed" aria-pressed="${s.view === 'dismissed'}">${safe('dismissed','Dismissed')} (${r?.dismissed_count ?? '—'})</button></div></div>
          <div class="hi-workspace"><div class="hi-inbox" aria-label="${esc(tr('incident_inbox','Incident inbox'))}">${(r?.incidents || []).map(i => `<article class="hi-inbox-row" data-hi-row="${esc(i.incident_id)}"><button type="button" data-hi-select="${esc(i.incident_id)}" aria-controls="hi-selected-detail" aria-expanded="false"><span class="hi-row-copy"><span class="hi-row-context">${esc(i.repository_name || tr('host_4a8231','Host'))}${i.deployment_name ? ` · ${esc(i.deployment_name)}` : ''}</span><strong>${esc(i.summary)}</strong></span><time>${esc(age(i.opened_at))}</time>${icon('chevron-down')}</button></article>`).join('')}
          <p class="hi-inbox-empty"${r?.incidents.length ? ' hidden' : ''}>${s.reportError ? tr('incident_reports_unavailable','Incident reports are unavailable.') : s.view === 'dismissed' ? tr('no_dismissed_incidents','No dismissed incidents.') : tr('no_incidents_escalated','No incidents escalated to you.')}</p>${r?.next_before ? `<button type="button" class="btn hi-load-more" data-hi-more>${safe('show_more_incidents','Show more incidents')}</button>` : ''}</div><div class="hi-detail-host"><p class="hi-select-prompt">${safe('select_incident_prompt','Select an incident to view its response and next step.')}</p></div></div>
        </div></section>`;
    }

    function drawIncidents(focus = null) {
      const old = main()?.querySelector('.hi-incidents');
      if (!old) return;
      old.outerHTML = incidentsMarkup();
      const root = main().querySelector('.hi-incidents');
      root.querySelector('#hi-incidents-toggle').onclick = () => {
        s.expanded = !s.expanded;
        drawIncidents('#hi-incidents-toggle');
      };
      root.querySelector('[data-hi-dismissed]')?.addEventListener('click', () => switchView('dismissed'));
      root.querySelectorAll('[data-hi-view]').forEach(b => b.onclick = () => switchView(b.dataset.hiView));
      root.querySelector('[data-hi-search]').oninput = e => { s.query = e.target.value; filterIncidents(); };
      root.querySelectorAll('[data-hi-select]').forEach(b => b.onclick = () => {
        s.selected = phone.matches && s.selected === b.dataset.hiSelect ? null : b.dataset.hiSelect;
        selectIncident(true);
      });
      root.querySelector('[data-hi-more]')?.addEventListener('click', async e => { e.target.disabled = true; await loadIncidents(true); });
      bindRetries(root);
      filterIncidents();
      if (s.expanded && !phone.matches && !s.selected) s.selected = visibleIncidents()[0]?.incident_id || null;
      selectIncident(false);
      if (focus) root.querySelector(focus)?.focus({ preventScroll: true });
    }
    function visibleIncidents() {
      const query = s.query.trim().toLocaleLowerCase();
      return (s.reports?.incidents || []).filter(i => !query || [i.summary, i.what_happened, i.repository_name, i.deployment_name].join(' ').toLocaleLowerCase().includes(query));
    }
    function filterIncidents() {
      const ids = new Set(visibleIncidents().map(i => i.incident_id));
      main().querySelectorAll('[data-hi-row]').forEach(row => { row.hidden = !ids.has(row.dataset.hiRow); });
      const empty = main().querySelector('.hi-inbox-empty');
      if (empty) { empty.hidden = ids.size > 0; empty.textContent = s.reportError ? tr('incident_reports_unavailable','Incident reports are unavailable.') : s.query ? tr('no_incidents_match','No incidents match your search.') : s.view === 'dismissed' ? tr('no_dismissed_incidents','No dismissed incidents.') : tr('no_incidents_escalated','No incidents escalated to you.'); }
      if (!ids.has(s.selected)) { s.selected = phone.matches ? null : visibleIncidents()[0]?.incident_id || null; selectIncident(false); }
    }
    function selectIncident(focus) {
      main()?.querySelector('#hi-selected-detail')?.remove();
      const i = s.reports?.incidents.find(i => i.incident_id === s.selected);
      main()?.querySelectorAll('[data-hi-row]').forEach(row => {
        const selected = row.dataset.hiRow === i?.incident_id;
        row.classList.toggle('selected', selected);
        row.querySelector('button').setAttribute('aria-expanded', String(selected));
      });
      const prompt = main()?.querySelector('.hi-select-prompt');
      if (prompt) prompt.hidden = !!i;
      if (!i) return;
      const target = phone.matches ? main().querySelector(`[data-hi-row="${CSS.escape(i.incident_id)}"]`) : main().querySelector('.hi-detail-host');
      target?.insertAdjacentHTML('beforeend', incidentDetail(i));
      const detail = main().querySelector('#hi-selected-detail');
      detail?.querySelector('[data-hi-disposition]')?.addEventListener('click', async e => {
        const button = e.currentTarget;
        button.disabled = true;
        try {
          const saved = await api('health.incident.update', { incident_id: i.incident_id, expected_revision: i.revision, status: button.dataset.hiDisposition }, false);
          if (saved.status !== button.dataset.hiDisposition) throw new Error(tr('stored_incident_not_confirmed','The stored incident did not confirm this change.'));
          s.selected = null;
          await loadIncidents();
          main()?.querySelector('[data-hi-select], #hi-incidents-toggle')?.focus({ preventScroll: true });
        } catch (e) { detail.querySelector('.hi-action-result').textContent = `Could not save: ${e.message}`; button.disabled = false; }
      });
      detail?.querySelector('[data-hi-resources]')?.addEventListener('click', e => {
        s.sort = e.currentTarget.dataset.hiResources;
        drawRepositories();
        main().querySelector('#hi-repositories-heading').focus();
      });
      if (focus) detail?.focus({ preventScroll: true });
    }
    phone.addEventListener('change', () => {
      const detail = main()?.querySelector('#hi-selected-detail');
      if (!detail) return;
      const focused = detail.contains(document.activeElement) ? document.activeElement : null;
      const target = phone.matches ? main().querySelector(`[data-hi-row="${CSS.escape(s.selected)}"]`) : main().querySelector('.hi-detail-host');
      target?.append(detail);
      focused?.focus({ preventScroll: true });
    });
    async function switchView(view) { s.expanded = true; s.view = view; s.selected = null; s.query = ''; await loadIncidents(); }
    async function loadIncidents(more = false) {
      const epoch = s.epoch;
      const read = ++s.incidentRead;
      try {
        const r = await api('health.incidents', { view: s.view, limit: 20, ...(more ? { before: s.reports.next_before } : {}) });
        if (epoch !== s.epoch || read !== s.incidentRead || !main()?.querySelector('.health-dashboard')) return;
        s.reports = more ? { ...r, incidents: [...s.reports.incidents, ...r.incidents] } : r;
        s.reportError = null;
      } catch (e) { if (e.code === 'stale' || read !== s.incidentRead || epoch !== s.epoch) return; s.reportError = e.message; }
      drawIncidents();
    }

    function families(rows) {
      const groups = new Map();
      rows.forEach(r => {
        const key = r.repository_source?.key || r.repository_id;
        if (!groups.has(key)) groups.set(key, { key, name: r.repository_source?.name || r.display_name, rows: [] });
        groups.get(key).rows.push(r);
      });
      return [...groups.values()].map(g => {
        const value = key => g.rows.every(r => Number.isFinite(r[key])) ? g.rows.reduce((a,r) => a+r[key], 0) : null;
        // Nested checkout scans overlap. Show the primary record's storage in
        // that case; the remaining exact values stay visible in Checkouts.
        const nested = g.rows.some(r => g.rows.some(p => p !== r && r.root_path?.startsWith(p.root_path?.replace(/\/$/,'') + '/')));
        const primary = [...g.rows].sort((a,b) => (a.root_path?.length || Infinity)-(b.root_path?.length || Infinity))[0];
        return { ...g, primary, cpu_percent: value('cpu_percent'), memory_bytes: value('memory_bytes'), storage_bytes: nested ? primary.storage_bytes : value('storage_bytes'), storageNote: nested ? tr('main_checkout','Main checkout') : '', deployments: g.rows.flatMap(r => r.deployments || []) };
      });
    }
    function repositoryRows() {
      return families(s.repos).filter(g => !s.repoQuery || [g.name,...g.rows.flatMap(r => [r.display_name,r.root_path])].join(' ').toLowerCase().includes(s.repoQuery.toLowerCase())).sort((a,b) => (b[s.sort] || 0)-(a[s.sort] || 0)||a.name.localeCompare(b.name));
    }
    function deploymentCounts(deployments) {
      const counts = new Map();
      deployments.forEach(d => counts.set(d.state,(counts.get(d.state)||0)+1));
      return [...counts].map(([state,count]) => badge(`${window.DevCoordinatorI18n.number(count)} ${stateLabel(state)}`,state==='running'?'ok':['failed','degraded'].includes(state)?'warn':'')).join(' ');
    }
    function deploymentLinks(row) {
      const groups = new Map();
      for (const d of row.deployments || []) {
        const key = [d.name,d.source,d.state].join('\0');
        if (!groups.has(key)) groups.set(key,[]);
        groups.get(key).push(d);
      }
      return [...groups.values()].map(group => {
        group.sort((a,b)=>String(b.updated_at||'').localeCompare(String(a.updated_at||'')));
        const d=group[0];
        return `<li><a href="#/deployments/${encodeURIComponent(d.deployment_id)}">${group.length>1?tr('latest','Latest '):''}${esc(d.name)}</a>${badge(`${window.DevCoordinatorI18n.number(group.length)} ${stateLabel(d.state)}`)}<span class="muted">${esc(d.source)}</span></li>`;
      }).join('');
    }
    function drawRepositories() {
      const root = main()?.querySelector('#hi-repository-rows');
      if (!root) return;
      const groups = repositoryRows();
      root.innerHTML = groups.map(g => `<article class="hi-repo-row"><div class="hi-repo-name"><strong>${esc(g.name)}</strong>${g.rows.length>1?`<span>${g.rows.length} ${safe('checkouts','checkouts')}</span>`:''}</div><div data-label="${esc(tr('cpu_db9a4c','CPU'))}">${pct(g.cpu_percent)}</div><div data-label="${esc(tr('memory_c3963a','Memory'))}">${bytes(g.memory_bytes)}</div><div data-label="${esc(tr('storage_a69c4d','Storage'))}">${bytes(g.storage_bytes)}${g.storageNote?`<small>${g.storageNote}</small>`:''}</div><div class="hi-repo-deployments">${deploymentCounts(g.deployments)||`<span class="muted">${safe('no_deployments_9790ef','No deployments')}</span>`}</div><details class="hi-repo-details" data-hi-repo="${esc(g.key)}"${s.expandedRepos.has(g.key)?' open':''}><summary>${safe('checkouts_deployments','Checkouts & deployments')}</summary>${g.rows.map(r=>`<div class="hi-checkout"><strong>${esc(r.display_name)}</strong><p>${esc(r.root_path||tr('root_unavailable','Root unavailable'))}</p><small>${safe('cpu_db9a4c','CPU')} ${pct(r.cpu_percent)} · ${safe('memory_c3963a','Memory')} ${bytes(r.memory_bytes)} · ${safe('storage_a69c4d','Storage')} ${bytes(r.storage_bytes)}</small><ul>${deploymentLinks(r)}</ul></div>`).join('')}</details></article>`).join('') || `<p class="hi-empty">${safe('no_repositories_match','No repositories match this view.')}</p>`;
      root.querySelectorAll('[data-hi-repo]').forEach(d=>d.ontoggle=()=>d.open?s.expandedRepos.add(d.dataset.hiRepo):s.expandedRepos.delete(d.dataset.hiRepo));
      main().querySelectorAll('[data-hi-sort]').forEach(b=>b.setAttribute('aria-pressed',String(b.dataset.hiSort===s.sort)));
    }
    function hostMarkup(h) {
      const labels = {
        cpu: tr('cpu_db9a4c', 'CPU'),
        memory: tr('memory_c3963a', 'Memory'),
        storage: tr('storage_a69c4d', 'Storage'),
        load: tr('system_load_f58135', 'System load'),
      };
      const metric=(title,value,note)=>`<div class="hi-capacity-cell"><span>${esc(title)}</span><strong>${value}</strong><small>${note}</small></div>`;
      const cores = tr('value1_cores_cfde1b', `${h.ncpu} cores`, { value1: h.ncpu });
      return `<div class="hi-host" data-ui-region="health-primary"><section class="hi-capacity"><h2>${safe('host_capacity_c36874','Host capacity')}</h2><div>${metric(labels.cpu,pct(h.cpu_percent),esc(cores))}${metric(labels.memory,bytes(h.memory_used),`${safe('of','of')} ${bytes(h.memory_total)}`)}${metric(labels.storage,bytes(h.fs_used),`${bytes(h.fs_free)} ${safe('free','free')}`)}${metric(labels.load,String(h.load_1 ?? '—'),safe('one_minute_average','1-minute average'))}</div></section><section class="hi-trends"><div class="hi-trends-heading"><h2>${safe('system_trends_eeef72','System trends')}</h2><div role="group" aria-label="${esc(tr('history_range','History range'))}">${Object.keys(ranges).map(r=>`<button class="btn" type="button" data-hi-range="${r}" aria-pressed="${s.range===r}">${r}</button>`).join('')}</div></div><div id="hi-history" aria-live="polite"><p>${safe('loading_history','Loading history…')}</p></div></section></div>`;
    }
    async function loadHistory() {
      const epoch = s.epoch;
      const range = s.range;
      const [minutes,points] = ranges[range];
      try {
        const results=await Promise.all(['cpu_percent','memory_used','storage_bytes'].map(metric=>api('health.history',{subject_kind:'host',subject_id:'host',metric,minutes,points})));
        if(epoch!==s.epoch||range!==s.range)return;
        history.set(range, {results});
        drawHistory();
      } catch(e){ if(e.code==='stale'||epoch!==s.epoch||range!==s.range)return;history.set(range,{error:e});drawHistory(); }
    }
    function drawHistory() {
      const el=main()?.querySelector('#hi-history'), saved=history.get(s.range);
      if(!el||!saved)return;
      if(saved.error){el.innerHTML=fault(tr('history_unavailable','History unavailable.')+' '+saved.error.message,'history');bindRetries(el);return;}
      el.innerHTML=saved.results.map((r,i)=>chart(r.points,i?pctBytes:pct,[tr('cpu_db9a4c','CPU'),tr('memory_c3963a','Memory'),tr('storage_a69c4d','Storage')][i], {scale:tr('scale','scale'),now:tr('now_ed5eb9','now')})).join('');
    }
    const pctBytes = n => bytes(n);
    function attribution(data) {
      const h=data.host,r=h.reconciliation;
      const labels={docker_shared:'Docker shared',docker_images:'Docker images',docker_build_cache:'Docker build cache',docker_shared_volumes:'Docker shared volumes',other:'Other'};
      return `<details class="hi-attribution"><summary>${safe('host_attribution','Host attribution')}</summary><p>${safe('managed_cpu','Managed CPU')} ${pct(r.managed_cpu_percent)} · ${safe('devcoordinator_b19f38','DevCoordinator')} ${pct(r.daemon_cpu_percent)} · ${safe('shared_unattributed_aa4d5b','Shared / unattributed')} ${pct(r.other_cpu_percent)}</p><p>${safe('managed_memory','Managed memory')} ${bytes(r.managed_memory)} · ${safe('devcoordinator_b19f38','DevCoordinator')} ${bytes(r.daemon_memory)} · ${safe('shared_unattributed_aa4d5b','Shared / unattributed')} ${bytes(r.other_memory)}</p><p>${safe('load_range','Load (1 / 5 / 15 minutes)')}: ${esc(h.load_1)} / ${esc(h.load_5)} / ${esc(h.load_15)} · ${safe('swap','Swap')} ${bytes(h.swap_used)}</p><dl class="hi-storage-breakdown">${Object.entries(labels).map(([k,label])=>`<div><dt>${safe(`label_storage_${k}`,label)}</dt><dd>${bytes(data.storage?.[k])}</dd></div>`).join('')}</dl></details>`;
    }
    function bindRetries(root) { root.querySelectorAll('[data-hi-retry]').forEach(b=>b.onclick=()=>b.dataset.hiRetry==='incidents'?loadIncidents():b.dataset.hiRetry==='history'?loadHistory():show()); }
    async function show() {
      const epoch=++s.epoch;
      summaryResult=null;
      history.clear();
      main().innerHTML=`<div class="health-dashboard">${heading()}<p role="status">${safe('loading_health','Loading Health…')}</p><div class="skeleton"></div></div>`;
      const results=await Promise.allSettled([api('health.summary',{}),api('health.repositories',{}),api('health.incidents',{view:s.view,limit:20})]);
      if(epoch!==s.epoch||!location.hash.startsWith('#/health')||location.hash.includes('/containers'))return;
      const [summary,repos,reports]=results;
      s.reports=reports.status==='fulfilled'?reports.value:null;
      s.reportError=reports.status==='rejected'?reports.reason.message:null;
      s.repos=repos.status==='fulfilled'?repos.value.repositories:[];
      summaryResult=summary;
      repositoryError=repos.status==='rejected'?repos.reason:null;
      drawDashboard();
      if(summary.status==='fulfilled')await loadHistory();
    }
    function drawDashboard() {
      const summary=summaryResult;
      const summaryMarkup=summary.status==='fulfilled'?hostMarkup(summary.value.host):fault((summary.reason.code==='permission_denied'?tr('host_health_admin_only','Host health is administrator-only.') + ' ':tr('host_health_unavailable','Host health unavailable.') + ' ')+summary.reason.message,'all');
      main().innerHTML=`<div class="health-dashboard">${heading()}${incidentsMarkup()}${summaryMarkup}<section class="hi-repositories" data-ui-region="health-repositories"><div class="hi-repositories-heading"><h2 id="hi-repositories-heading" tabindex="-1">${safe('resources_by_repository','Resources by repository')}</h2><label><span class="sr-only">${safe('search_repositories','Search repositories')}</span><input type="search" data-hi-repo-search placeholder="${esc(tr('find_repository','Find repository'))}" value="${esc(s.repoQuery)}"></label></div><div class="hi-repo-head"><span>${safe('repository_13d6ff','Repository')}</span>${['cpu_percent','memory_bytes','storage_bytes'].map((k,i)=>`<button type="button" data-hi-sort="${k}" aria-pressed="${s.sort===k}">${safe(['cpu_db9a4c','memory_c3963a','storage_a69c4d'][i],['CPU','Memory','Storage'][i])}</button>`).join('')}<span>${safe('deployments_842a46','Deployments')}</span></div>${repositoryError?fault(tr('repository_resources_unavailable','Repository resources unavailable.') + ' '+repositoryError.message,'all'):''}<div id="hi-repository-rows"></div></section>${summary.status==='fulfilled'?attribution(summary.value):''}</div>`;
      main().querySelector('[data-hi-refresh]').onclick=show;
      main().querySelectorAll('[data-hi-sort]').forEach(b=>b.onclick=()=>{s.sort=b.dataset.hiSort;drawRepositories();});
      main().querySelector('[data-hi-repo-search]').oninput=e=>{s.repoQuery=e.target.value;drawRepositories();};
      main().querySelectorAll('[data-hi-range]').forEach(b=>b.onclick=()=>{s.range=b.dataset.hiRange;main().querySelectorAll('[data-hi-range]').forEach(x=>x.setAttribute('aria-pressed',String(x===b)));loadHistory();});
      drawIncidents();drawRepositories();bindRetries(main());
      drawHistory();
    }
    document.addEventListener('dc2:localechange', () => {
      if(!summaryResult||!main()?.querySelector('.health-dashboard')||!location.hash.startsWith('#/health')||location.hash.includes('/containers'))return;
      const root=main(), focused=document.activeElement, scroll={x:scrollX,y:scrollY};
      const inputs=[...root.querySelectorAll('input[data-hi-search],input[data-hi-repo-search]')];
      const expanded=[...root.querySelectorAll('details[open]')].map(node=>node.dataset.hiRepo ? `[data-hi-repo="${CSS.escape(node.dataset.hiRepo)}"]` : node.classList.contains('hi-attribution') ? '.hi-attribution' : '.hi-technical');
      const selected=inputs.includes(focused) ? {start:focused.selectionStart,end:focused.selectionEnd,direction:focused.selectionDirection} : null;
      const focusAttribute=root.contains(focused) ? [...focused.attributes].find(attribute=>attribute.name.startsWith('data-hi-')) : null;
      const focusSelector=focusAttribute ? `[${focusAttribute.name}="${CSS.escape(focusAttribute.value)}"]` : null;
      const inboxScroll=root.querySelector('.hi-inbox')?.scrollTop;
      drawDashboard();
      // Keep the actual search controls, their drafts, selection and focus.
      for(const input of inputs){
        const selector=input.hasAttribute('data-hi-search')?'[data-hi-search]':'[data-hi-repo-search]';
        const replacement=root.querySelector(selector);
        if(replacement){input.placeholder=replacement.placeholder;replacement.replaceWith(input);}
      }
      expanded.forEach(selector=>{const node=root.querySelector(selector);if(node)node.open=true;});
      if(selected){focused.focus({preventScroll:true});focused.setSelectionRange(selected.start,selected.end,selected.direction);}
      else if(focusSelector)root.querySelector(focusSelector)?.focus({preventScroll:true});
      const inbox=root.querySelector('.hi-inbox');if(inbox&&inboxScroll!=null)inbox.scrollTop=inboxScroll;
      scrollTo(scroll.x,scroll.y);
    });
    return {show};
  }
  window.DevCoordinatorHealth = { create };
})();
