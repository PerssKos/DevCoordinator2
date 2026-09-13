// DevCoordinator2 Console: a static app over the edge's /api/v2/<operation> bridge.
// Every control calls the real API and re-reads state to prove the change.
'use strict';

const $ = (sel, root = document) => root.querySelector(sel);
const main = $('#main');
const state = {
  who: null,
  healthRange: '24h',
  usageRange: '1h',
  codexUsageRange: '24h',
  progressPeriod: 'day',
  progressRepositoryId: null,
  progressSelectedTaskId: null,
  decisionAspect: 'all',
  decisionLimit: 25,
  decisionBefore: null,
  decisionQuery: '',
  collapsed: new Set(),
  planRepositoryId: null,
  planSelectedTaskId: null,
  planMode: 'select',
  planZoom: 1,
  planFit: false,
  planNavigatorCollapsed: false,
  planNavigatorWidth: 380,
  planSelectionCollapsed: false,
  planScrollLeft: 0,
  planScrollTop: 0,
  planRequestedTaskId: null,
  evidenceRunId: null,
  evidenceRun: null,
  evidenceData: null,
  evidenceSteps: [],
  evidenceStepKey: null,
  evidenceViewport: null,
  evidenceScreenshotKind: 'viewport',
  evidenceTool: 'select',
  evidenceColor: '#f59e0b',
  evidenceZoom: 1,
  evidenceDraftMarks: [],
  evidenceDraftBody: '',
  evidenceDraftKey: null,
  evidenceDrafts: new Map(),
  evidenceSavingKey: null,
  evidenceComposerDismissed: false,
  evidencePendingLabel: null,
  evidenceUndo: [],
  evidenceRedo: [],
  evidenceSelectedMarkId: null,
  evidenceSelectedFeedbackId: null,
  evidenceImageUrls: new Map(),
  evidenceImagePromises: new Map(),
  collapsedDeploymentRepositories: new Set(),
  collapsedDeploymentWorkers: new Set(),
  deploymentUsageResolutions: new Map(),
};
const RANGES = {
  '1h': { minutes: 60, points: 60 },
  '24h': { minutes: 1440, points: 288 },
  '7d': { minutes: 10080, points: 336 },
  '30d': { minutes: 43200, points: 360 },
};

function esc(value) {
  return String(value ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}
function bytes(n) {
  if (n == null || Number.isNaN(Number(n))) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let v = Number(n); let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i += 1; }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}
function pct(n) { return n == null ? '—' : `${Number(n).toFixed(1)}%`; }
function ago(iso) {
  if (!iso) return '—';
  const s = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  if (s < 90) return `${Math.round(s)}s ago`;
  if (s < 5400) return `${Math.round(s / 60)}m ago`;
  if (s < 172800) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}
function until(iso) {
  if (!iso) return '—';
  const s = (Date.parse(iso) - Date.now()) / 1000;
  if (s <= 0) return 'Expired';
  if (s < 90) return `in ${Math.round(s)}s`;
  if (s < 5400) return `in ${Math.round(s / 60)}m`;
  if (s < 172800) return `in ${Math.round(s / 3600)}h`;
  return `in ${Math.round(s / 86400)}d`;
}
function badge(text, kind) {
  const cls = kind || ({ running: 'ok', healthy: 'ok', passed: 'ok', stopped: '', degraded: 'warn', unhealthy: 'bad', failed: 'bad', 'timed-out': 'bad', cancelled: '', interrupted: 'warn', superseded: '', applying: 'warn', unknown: '', none: '' }[text] ?? '');
  return `<span class="badge ${cls}">${esc(text)}</span>`;
}

function destinationLink(label, href) {
  return `<a class="destination-link" href="${esc(href)}">${esc(label)}</a>`;
}

function pageHeading(label, href, current = '', trailing = '') {
  return `<h1 class="page-heading">${destinationLink(label, href)}${current ? `<span class="context-slash" aria-hidden="true">/</span><strong>${esc(current)}</strong>` : ''}${trailing}</h1>`;
}

function normalizedProjects(projects) {
  const byId = new Map();
  for (const project of projects || []) {
    const id = project.repository_id;
    const label = project.display_name;
    if (typeof id === 'string' && id && typeof label === 'string' && label && !byId.has(id)) {
      byId.set(id, { repository_id: id, display_name: label });
    }
  }
  return [...byId.values()].sort((left, right) => left.display_name.localeCompare(right.display_name));
}

function projectPicker(projects, currentId, hrefFor, pickerId) {
  if (workspace.active) return '';
  const options = normalizedProjects(projects);
  const current = options.find((project) => project.repository_id === currentId)
    || { repository_id: currentId, display_name: 'Current project' };
  const menuId = `${pickerId}-project-menu`;
  return `<span class="project-picker" data-project-picker>
    <strong class="project-picker-current">${esc(current.display_name)}</strong>
    <button type="button" class="project-picker-toggle" aria-label="Choose project. Current project: ${esc(current.display_name)}" aria-haspopup="menu" aria-controls="${esc(menuId)}" aria-expanded="false" data-project-picker-toggle>${planIcon('chevron-down')}</button>
    <span class="project-picker-menu" id="${esc(menuId)}" role="menu" aria-label="Projects" data-project-picker-menu hidden>
      ${options.map((project) => `<a role="menuitem" href="${esc(hrefFor(project.repository_id))}"${project.repository_id === currentId ? ' aria-current="page"' : ''}>${esc(project.display_name)}${project.repository_id === currentId ? '<span class="project-picker-selected">Current</span>' : ''}</a>`).join('')}
    </span>
  </span>`;
}

let closeActiveProjectPicker = null;
function bindProjectPicker(root = main) {
  root.querySelectorAll('[data-project-picker]').forEach((picker) => {
    const toggle = $('[data-project-picker-toggle]', picker);
    const menu = $('[data-project-picker-menu]', picker);
    if (!toggle || !menu) return;
    const items = () => [...menu.querySelectorAll('[role="menuitem"]')];
    const close = (restoreFocus = false) => {
      menu.hidden = true;
      toggle.setAttribute('aria-expanded', 'false');
      picker.classList.remove('open');
      document.removeEventListener('pointerdown', outside, true);
      if (closeActiveProjectPicker === close) closeActiveProjectPicker = null;
      if (restoreFocus) toggle.focus();
    };
    const outside = (event) => { if (!picker.contains(event.target)) close(false); };
    const open = () => {
      closeActiveProjectPicker?.(false);
      menu.hidden = false;
      toggle.setAttribute('aria-expanded', 'true');
      picker.classList.add('open');
      closeActiveProjectPicker = close;
      document.addEventListener('pointerdown', outside, true);
      requestAnimationFrame(() => (menu.querySelector('[aria-current="page"]') || items()[0])?.focus());
    };
    toggle.addEventListener('click', () => menu.hidden ? open() : close(true));
    toggle.addEventListener('keydown', (event) => {
      if (!menu.hidden || !['ArrowDown', 'ArrowUp'].includes(event.key)) return;
      event.preventDefault(); open();
      requestAnimationFrame(() => {
        const links = items();
        links[event.key === 'ArrowUp' ? links.length - 1 : 0]?.focus();
      });
    });
    menu.addEventListener('click', (event) => {
      if (event.target.closest('[role="menuitem"]')) close(false);
    });
    picker.addEventListener('keydown', (event) => {
      if (event.key === 'Escape' && !menu.hidden) {
        event.preventDefault(); close(true); return;
      }
      if (menu.hidden || !['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
      event.preventDefault();
      const links = items(); const selected = links.indexOf(document.activeElement);
      const next = event.key === 'Home' ? 0 : event.key === 'End' ? links.length - 1
        : event.key === 'ArrowDown' ? (selected + 1 + links.length) % links.length
          : (selected - 1 + links.length) % links.length;
      links[next]?.focus();
    });
  });
}
function spark(values, width = 90, height = 20) {
  if (!values || values.length < 2) return '<span class="muted">—</span>';
  const max = Math.max(...values, 1e-9); const step = width / (values.length - 1);
  const pts = values.map((v, i) => `${(i * step).toFixed(1)},${(height - (v / max) * (height - 2) - 1).toFixed(1)}`).join(' ');
  return `<svg class="spark" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" aria-label="trend"><polyline fill="none" stroke="#4c8dff" stroke-width="1.5" points="${pts}"/></svg>`;
}
function meter(fraction) {
  if (fraction == null || Number.isNaN(fraction)) return '';
  const p = Math.max(0, Math.min(100, fraction * 100));
  const cls = fraction >= 0.9 ? 'bad' : fraction >= 0.75 ? 'warn' : '';
  return `<div class="meter"><span class="${cls}" style="width:${p.toFixed(1)}%"></span></div>`;
}
function minuteLabel(minute) {
  return minute ? `${minute.slice(5, 10)} ${minute.slice(11, 16)}` : '';
}
// Time-series chart: shaded min–max envelope plus the average line, with the
// scale and window bounds as plain HTML so nothing distorts or clips.
function chart(points, fmt, label) {
  if (!points || points.length < 2) {
    return `<div class="chartbox"><div class="chartmeta"><span>${esc(label)}</span></div><div class="notice muted">No history for this window yet. Samples accumulate while the coordinator runs.</div></div>`;
  }
  const w = 600; const h = 130; const T = 4; const B = 4;
  const ih = h - T - B;
  const maxV = Math.max(...points.map((p) => p.max ?? p.avg), 1e-9) * 1.05;
  const X = (i) => ((i / (points.length - 1)) * w);
  const Y = (v) => T + ih - (Math.min(v, maxV) / maxV) * ih;
  const upper = points.map((p, i) => [X(i), Y(p.max ?? p.avg)]);
  const lower = points.map((p, i) => [X(i), Y(p.min ?? p.avg)]);
  const band = [...upper, ...lower.reverse()].map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(' ');
  const avg = points.map((p, i) => `${X(i).toFixed(1)},${Y(p.avg).toFixed(1)}`).join(' ');
  const grid = [0.25, 0.5, 0.75].map((f) => `<line x1="0" x2="${w}" y1="${Y(maxV * f).toFixed(1)}" y2="${Y(maxV * f).toFixed(1)}" class="gridline"/>`).join('');
  const last = points.at(-1);
  return `<div class="chartbox">
    <div class="chartmeta"><span>${esc(label)}</span><span><span class="muted">scale 0–${fmt(maxV)} · now</span> <strong>${fmt(last.avg)}</strong></span></div>
    <svg class="chart" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" aria-label="${esc(label)} history">${grid}<polygon points="${band}" class="band"/><polyline points="${avg}" class="line" fill="none"/></svg>
    <div class="chartaxis"><span>${esc(minuteLabel(points[0].minute))}</span><span class="muted">min–max band, average line</span><span>${esc(minuteLabel(last.minute))}</span></div>
  </div>`;
}
function seg(options, current, dataKey, label = (o) => o) {
  return `<div class="seg" role="tablist">${options.map((o) => `<button type="button" class="${o === current ? 'active' : ''}" data-${dataKey}="${o}">${esc(label(o))}</button>`).join('')}</div>`;
}
function bindSeg(root, dataKey, apply) {
  const prop = dataKey.replace(/-([a-z])/g, (_, c) => c.toUpperCase());
  root.querySelectorAll(`[data-${dataKey}]`).forEach((btn) => btn.addEventListener('click', () => apply(btn.dataset[prop])));
}
function toast(text, kind = '') {
  const el = document.createElement('div');
  el.className = `toast ${kind}`; el.textContent = text;
  $('#toasts').appendChild(el);
  setTimeout(() => el.remove(), 6000);
}

class ApiError extends Error { constructor(code, message) { super(message); this.code = code; } }
let viewAbort = null; // render() aborts the previous view's pending reads so a slow stale load can never overwrite the current view
async function api(operation, params = {}, abortable = true) {
  let res;
  try {
    res = await fetch(`/api/v2/${operation}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(params), signal: abortable ? viewAbort?.signal : undefined });
  } catch (error) {
    if (error.name === 'AbortError') throw new ApiError('stale', 'superseded by navigation');
    throw new ApiError('network', `edge unreachable: ${error.message}`);
  }
  if (res.status === 401) { location.href = `/auth/login?rt=${encodeURIComponent(location.pathname + location.hash)}`; throw new ApiError('unauthenticated', 'sign in'); }
  const body = await res.json().catch(() => ({ ok: false, error: { code: 'bad_response', message: `HTTP ${res.status}` } }));
  if (!body.ok) throw new ApiError(body.error?.code || 'error', body.error?.message || 'request failed');
  if (operation === 'test.list' && !params.after_worktree_id && !params.limit) {
    const result = body.data;
    const seen = new Set();
    while (result.next_worktree_id) {
      const cursor = result.next_worktree_id;
      if (seen.has(cursor)) throw new ApiError('bad_response', 'Test list pagination did not advance');
      seen.add(cursor);
      const page = await api(operation, { ...params, after_worktree_id: cursor }, abortable);
      result.runs.push(...page.runs);
      result.next_worktree_id = page.next_worktree_id;
    }
  }
  return body.data;
}
async function metricHistory(kind, id, metric, rangeKey) {
  const r = RANGES[rangeKey] || RANGES['24h'];
  return api('health.history', { subject_kind: kind, subject_id: id, metric, minutes: r.minutes, points: r.points });
}

function setBanner(text) { const b = $('#banner'); b.hidden = !text; b.textContent = text || ''; }
function skeleton(rows = 4) { return Array.from({ length: rows }, () => '<div class="skeleton"></div>').join(''); }
function stateBlock(kind, text) {
  if (kind === 'denied') return `<div class="notice denied">Permission denied: ${esc(text)}</div>`;
  if (kind === 'error') return `<div class="notice"><strong>Could not load.</strong> ${esc(text)} <button class="btn btn-small" onclick="render()">Retry</button></div>`;
  return `<div class="notice muted">${esc(text)}</div>`;
}
function currentDestinationHeading() {
  const [, view, arg] = (location.hash || '#/plan').split('?')[0].slice(1).split('/');
  const destinations = {
    deployments: ['Deployments', '#/deployments'], plan: ['Plan', '#/plan'],
    progress: ['Progress', '#/progress'], usage: ['Codex Usage', '#/usage'],
    decisions: ['Decisions', '#/decisions'],
    glossary: ['Glossary', '#/glossary'],
    tests: ['Tests', '#/tests'], health: ['Health', '#/health'],
    bugs: ['Bugs', '#/bugs'], admin: ['Administration', '#/admin'],
  };
  const destination = destinations[view] || destinations.deployments;
  return pageHeading(destination[0], destination[1], view === 'health' && arg === 'containers' ? 'Containers' : '');
}
function guard(fn) {
  return async (...args) => {
    try { return await fn(...args); } catch (error) {
      if (error.code === 'stale') return null; // another view took over
      if (error.code === 'permission_denied') main.innerHTML = currentDestinationHeading() + stateBlock('denied', error.message);
      else if (['test_evidence_expired', 'test_evidence_not_found'].includes(error.code)) main.innerHTML = currentDestinationHeading() + stateBlock('empty', 'No visual evidence is available for this run. It may have expired.');
      else if (error.code !== 'unauthenticated') main.innerHTML = currentDestinationHeading() + stateBlock('error', error.message);
      return null;
    }
  };
}
async function act(button, command, args, after) {
  button.disabled = true;
  try {
    const result = await api(command, args, false);
    toast(`${command}: ${result.state ?? result.status ?? result.domain ?? 'done'}`, 'ok');
    if (after) await after(result);
  } catch (error) {
    toast(`${command} failed: ${error.message}`, 'bad');
  } finally { button.disabled = false; }
}
function bind(root) {
  root.querySelectorAll('[data-cmd]').forEach((btn) => {
    if (btn.dataset.commandBound === 'true') return;
    btn.dataset.commandBound = 'true';
    btn.addEventListener('click', () => {
      const args = JSON.parse(btn.dataset.args || '{}');
      act(btn, btn.dataset.cmd, args, () => render());
    });
  });
}

function setupTopNavigation() {
  const header = $('.top'); const toggle = $('#nav-toggle'); const nav = $('#nav');
  if (!header || !toggle || !nav) return;
  const setOpen = (open, restoreFocus = false) => {
    header.classList.toggle('nav-open', open);
    toggle.setAttribute('aria-expanded', String(open));
    toggle.setAttribute('aria-label', open ? 'Close navigation' : 'Open navigation');
    if (open) requestAnimationFrame(() => (nav.querySelector('a.active:not([hidden])') || nav.querySelector('a:not([hidden])'))?.focus());
    else if (restoreFocus) toggle.focus();
  };
  toggle.addEventListener('click', () => setOpen(!header.classList.contains('nav-open'), true));
  nav.addEventListener('click', (event) => {
    if (event.target.closest('a')) setOpen(false, false);
  });
  document.addEventListener('pointerdown', (event) => {
    if (header.classList.contains('nav-open') && !header.contains(event.target)) setOpen(false, false);
  }, true);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && header.classList.contains('nav-open')) {
      event.preventDefault(); setOpen(false, true);
    }
  });
  const desktop = window.matchMedia('(min-width: 1241px)');
  desktop.addEventListener('change', (event) => { if (event.matches) setOpen(false, false); });
}
function lifecycleButtons(id, component, cls = 'btn btn-small', deploymentState = null) {
  const args = component ? { deployment_id: id, component } : { deployment_id: id };
  const canOperate = state.who?.administrator || ['operator', 'administrator'].includes(state.who?.grants?.[id]);
  const blocked = !canOperate ? ' disabled aria-disabled="true" title="Operator access required"' : deploymentState === 'applying' ? ' disabled aria-disabled="true" title="Apply in progress; refresh status before another lifecycle action"' : '';
  return ['start', 'stop', 'restart'].map((a) => `<button class="${cls}" data-cmd="deployment.${a}" data-args='${esc(JSON.stringify(args))}'${blocked}>${a}</button>`).join('');
}

// --- Deployments ---------------------------------------------------------
async function optionalDashboardRead(command, args = {}) {
  try {
    return { value: await api(command, args) };
  } catch (error) {
    if (error.code === 'stale') throw error;
    return { error };
  }
}

function rowsByRepository(result) {
  return new Map((result?.value?.repositories || []).map((row) => [row.repository_id, row]));
}

function repositoryOperatorAllowed(deployments) {
  if (state.who?.local || state.who?.administrator) return true;
  const grants = state.who?.grants || {};
  return deployments.some((deployment) => ['operator', 'administrator'].includes(grants[deployment.deployment_id]));
}

function repositoryOverallStatus(deployments) {
  const attention = deployments.filter((deployment) => ['degraded', 'failed'].includes(deployment.state)
    || deployment.health === 'unhealthy');
  if (attention.length) {
    const degraded = deployments.filter((deployment) => deployment.state === 'degraded').length;
    const label = degraded === attention.length
      ? `${degraded} degraded`
      : `${attention.length} need attention`;
    return { text: `Attention · ${label}`, kind: 'warn' };
  }
  const applying = deployments.filter((deployment) => deployment.state === 'applying').length;
  if (applying) return { text: `Updating · ${applying} applying`, kind: 'warn' };
  const stopped = deployments.filter((deployment) => deployment.state === 'stopped').length;
  if (stopped) return { text: `Stopped · ${stopped}`, kind: '' };
  const observed = deployments.every((deployment) => deployment.observed_only);
  const explicitlyHealthy = deployments.every((deployment) => deployment.health === 'healthy');
  if (observed && explicitlyHealthy) return { text: 'Healthy · observed', kind: 'ok' };
  if (deployments.every((deployment) => deployment.state === 'running')) {
    return { text: observed ? 'Running · observed' : 'Running', kind: 'ok' };
  }
  return { text: 'Status unavailable', kind: '' };
}

function dashboardLink(label, href, repositoryName) {
  if (!href) return '';
  return `<a class="deployment-summary-link" href="${esc(href)}" aria-label="${esc(`${label} for ${repositoryName}`)}"><span>${esc(label)}</span>${planIcon('arrow-right')}</a>`;
}

function deploymentSummaryItem({ label, summaryKey = '', value, note = '', facts = [], kind = '', href = '', link, repositoryName, live = false, busy = false }) {
  const key = summaryKey || label.toLowerCase().replaceAll(' ', '-');
  return `<div class="deployment-summary-item" data-summary="${esc(key)}"${live ? ' aria-live="polite"' : ''}${busy ? ' aria-busy="true"' : ''}>
    <span class="deployment-summary-label">${esc(label)}</span>
    <strong class="deployment-summary-value ${esc(kind)}">${esc(value)}</strong>
    ${note ? `<small data-summary-note>${esc(note)}</small>` : ''}
    ${facts.length ? `<dl class="deployment-summary-facts">${facts.map(([name, fact]) => `<div><dt>${esc(name)}</dt><dd>${esc(fact)}</dd></div>`).join('')}</dl>` : ''}
    ${dashboardLink(link, href, repositoryName)}
  </div>`;
}

function deploymentRecord(deployment, admin) {
  const health = deployment.health && deployment.health !== 'unknown'
    && deployment.health !== deployment.state ? badge(deployment.health) : '';
  const domain = deployment.domain ? esc(deployment.domain) : '<span class="muted">—</span>';
  const stateKind = ['degraded', 'failed'].includes(deployment.state)
    || deployment.health === 'unhealthy' ? 'attention' : deployment.state === 'applying' ? 'applying' : 'normal';
  return `<article class="deployment-record ${stateKind}" data-deployment-id="${esc(deployment.deployment_id)}">
    <div class="deployment-record-identity">
      <div class="deployment-record-title"><a href="#/deployments/${esc(deployment.deployment_id)}"><strong>${esc(deployment.name)}@${esc(deployment.source)}</strong></a></div>
      <span class="muted mono">${esc(deployment.deployment_id)}</span>
    </div>
    <div class="deployment-record-status" aria-label="Deployment status">${badge(deployment.state)} ${health} ${deployment.observed_only ? badge('observed') : ''}</div>
    <div class="deployment-record-body"><dl class="deployment-record-facts deployment-record-endpoint">
      <div><dt>Domain</dt><dd><span class="deployment-domain">${domain}</span>${admin ? ` <button class="btn btn-small deployment-domain-edit" data-edit-domain="${esc(deployment.deployment_id)}" aria-label="Edit domain for ${esc(deployment.name)}@${esc(deployment.source)}">edit</button>` : ''}</dd></div>
      <div><dt>Port</dt><dd>${deployment.route_port ?? '—'}</dd></div>
    </dl>
    <dl class="deployment-record-facts deployment-record-runtime">
      <div><dt>Generation</dt><dd>${deployment.current_generation ?? '—'}</dd></div>
      <div><dt>Updated</dt><dd>${ago(deployment.updated_at)}</dd></div>
    </dl>
    <div class="deployment-record-actions"><span>Actions</span><div class="actions">${lifecycleButtons(deployment.deployment_id, null, 'btn btn-small', deployment.state)}
      ${!deployment.observed_only && admin ? `<button class="btn btn-small" data-cmd="deployment.apply" data-args='${esc(JSON.stringify({ deployment_id: deployment.deployment_id }))}'${deployment.state === 'applying' ? ' disabled aria-disabled="true" title="Apply already in progress"' : ''}>apply</button>` : ''}</div></div></div>
  </article>`;
}

function dashboardUsageDisplay(usage, canOperate, sourceError = null, resolving = false) {
  if (!canOperate) return { value: 'Operator access required', note: '' };
  const totals = usage?.totals || usage;
  const coverage = usage?.coverage;
  if (totals?.total_tokens != null) {
    return {
      value: `${compactNumber(totals.total_tokens)} tokens · ${Number(totals.model_requests || 0).toLocaleString('en-US')} requests`,
      note: coverage?.snapshot ? usageSnapshotText(coverage) : coverage ? coverageText(coverage, true) : '',
    };
  }
  if (resolving && coverage?.unavailable_reasons?.mapping_pending) {
    return { value: 'Loading usage…', note: '' };
  }
  if (coverage) return { value: coverageText(coverage, true), note: '' };
  if (sourceError) return { value: 'Usage unavailable', note: '' };
  return { value: 'No measured usage', note: '' };
}

function pendingDashboardUsage(usage, canOperate) {
  return Boolean(canOperate && (usage?.coverage?.snapshot?.refreshing
    || (usage?.total_tokens == null && usage?.coverage?.unavailable_reasons?.mapping_pending)));
}

function resolveDashboardUsage(repositoryId) {
  const existing = state.deploymentUsageResolutions.get(repositoryId);
  if (existing) return existing;
  const resolution = optionalDashboardRead('usage.repository', {
    repository_id: repositoryId, range: '24h', wait_for_refresh: true,
  });
  state.deploymentUsageResolutions.set(repositoryId, resolution);
  resolution.then(
    () => state.deploymentUsageResolutions.delete(repositoryId),
    () => state.deploymentUsageResolutions.delete(repositoryId),
  );
  return resolution;
}

function updateDeploymentUsageCard(card, display) {
  const value = card.querySelector('.deployment-summary-value');
  if (value) value.textContent = display.value;
  let note = card.querySelector('[data-summary-note]');
  if (display.note && !note) {
    note = document.createElement('small');
    note.dataset.summaryNote = '';
    card.insertBefore(note, card.querySelector('.deployment-summary-link'));
  }
  if (note) {
    note.textContent = display.note;
    note.hidden = !display.note;
  }
  card.removeAttribute('aria-busy');
}

async function hydratePendingDeploymentUsage(groups, usageRows) {
  for (const group of groups) {
    const canOperate = repositoryOperatorAllowed(group.deployments);
    if (!pendingDashboardUsage(usageRows.get(group.repositoryId), canOperate)) continue;
    let result;
    try {
      result = await resolveDashboardUsage(group.repositoryId);
    } catch (error) {
      if (error.code === 'stale') return;
      result = { error };
    }
    const [, view] = (location.hash || '#/deployments').slice(1).split('/');
    if (view !== 'deployments') return;
    const section = main.querySelector(`.deployment-repository[data-repository-id="${CSS.escape(group.repositoryId)}"]`);
    const card = section?.querySelector('[data-summary="usage"]');
    if (!card) continue;
    updateDeploymentUsageCard(card, dashboardUsageDisplay(result.value, canOperate, result.error));
    if (result.value?.coverage?.snapshot?.refreshing) {
      queueMicrotask(() => hydratePendingDeploymentUsage([group], new Map([[group.repositoryId, result.value]])));
    }
  }
}

function dashboardTestElapsed(test) {
  if (test.duration_seconds != null) return durationMs(Number(test.duration_seconds) * 1000);
  const started = Date.parse(test.started_at || '');
  return test.status === 'running' && Number.isFinite(started) ? durationMs(Date.now() - started) : '—';
}

function repositoryDashboardSection(group, sources, decisions, admin, index) {
  const { repositoryId, repositoryName, deployments } = group;
  const plan = sources.plan.get(repositoryId);
  const progress = sources.progress.get(repositoryId);
  const usage = sources.usage.get(repositoryId);
  const tests = (sources.tests.value?.runs || []).filter((run) => run.repository_id === repositoryId);
  const health = sources.health.get(repositoryId);
  const decision = decisions.get(repositoryId)?.value?.decisions?.at(-1);
  const overall = repositoryOverallStatus(deployments);
  const canOperate = repositoryOperatorAllowed(deployments);
  const canReadTests = state.who?.local || state.who?.administrator;

  let planValue = 'Not recorded';
  if (plan?.current_release) planValue = `${plan.current_release.name} · ${plan.open_tasks} open`;
  else if (plan?.open_tasks) planValue = `${plan.open_tasks} open in backlog`;
  else if (plan) planValue = 'No open plan work';
  else if (sources.plan.error?.code === 'permission_denied') planValue = 'Access required';
  else if (sources.plan.error) planValue = 'Plan unavailable';

  let progressValue = 'No measured progress';
  if (!canOperate) progressValue = 'Operator access required';
  else if (progress?.planned_lines_total) {
    progressValue = `${Math.round(progress.planned_lines_done / progress.planned_lines_total * 100)}% · ${Number(progress.planned_lines_done).toLocaleString('en-US')} of ${Number(progress.planned_lines_total).toLocaleString('en-US')} lines`;
  } else if (sources.progress.error) progressValue = 'Progress unavailable';

  const usagePending = pendingDashboardUsage(usage, canOperate);
  const usageDisplay = dashboardUsageDisplay(usage, canOperate, sources.usage.error, usagePending);

  let testValue = 'No current run'; let testNote = ''; let testFacts = []; let testKind = '';
  if (!canReadTests) testValue = 'Administrator access required';
  else if (sources.tests.error) testValue = 'Tests unavailable';
  else if (tests.length) {
    const test = tests.find((run) => run.status === 'running')
      || tests.find((run) => ['failed', 'timed-out', 'interrupted'].includes(run.status)) || tests[0];
    testValue = `${test.test} · ${test.status}`;
    testNote = `${test.readiness_eligible ? 'Readiness proof' : 'Diagnostic run'} · started ${ago(test.started_at)}`;
    const outputObserved = test.stdout_bytes_observed == null && test.stderr_bytes_observed == null
      ? null : Number(test.stdout_bytes_observed || 0) + Number(test.stderr_bytes_observed || 0);
    testFacts = [
      ['Tier', testTierLabel(test.requested_tier)],
      ['Elapsed', dashboardTestElapsed(test)],
      ['Output', outputObserved == null ? '—' : bytes(outputObserved)],
    ];
    testKind = test.status === 'running' || test.status === 'passed' ? 'ok'
      : ['failed', 'timed-out'].includes(test.status) ? 'bad' : 'warn';
  }

  let healthValue = 'No health record'; let healthFacts = []; let healthKind = '';
  if (sources.healthResult.error) healthValue = 'Health unavailable';
  else if (health) {
    const healthName = health.health === 'healthy' && deployments.length === 1
      ? 'Deployment healthy'
      : health.health && health.health !== 'none' ? healthLabel(health.health, {}) : 'No incidents recorded';
    healthValue = `${healthName}${health.health === 'healthy' || health.cpu_percent == null ? '' : ` · CPU ${pct(health.cpu_percent)}`}`;
    healthFacts = [
      ['CPU', pct(health.cpu_percent)],
      ['Memory', bytes(health.memory_bytes)],
      ['Storage', bytes(health.storage_bytes)],
      ['Deployments', String(health.deployments?.length ?? deployments.length)],
    ];
    healthKind = health.health === 'unhealthy' ? 'bad' : health.health === 'healthy' ? 'ok' : '';
  }

  let decisionValue = 'No history'; let decisionNote = '';
  const decisionResult = decisions.get(repositoryId);
  if (decision) { decisionValue = decision.title; decisionNote = ago(decision.created_at); }
  else if (decisionResult?.error?.code === 'permission_denied') decisionValue = 'Access required';
  else if (decisionResult?.error && decisionResult.error.code !== 'repository_not_found') decisionValue = 'Decisions unavailable';

  const planHref = repositoryId ? `#/plan/${repositoryId}` : '';
  const progressHref = repositoryId && canOperate ? `#/progress/${repositoryId}` : '';
  const usageHref = repositoryId && canOperate ? `#/usage/${repositoryId}` : '';
  const decisionsHref = repositoryId ? `#/decisions/${repositoryId}` : '';
  const summary = [
    deploymentSummaryItem({ label: 'Plan', value: planValue, href: planHref, link: 'Open Plan', repositoryName }),
    deploymentSummaryItem({ label: 'Progress', value: progressValue, href: progressHref, link: 'Open Progress', repositoryName }),
    deploymentSummaryItem({ label: 'Usage · 24h', summaryKey: 'usage', value: usageDisplay.value, note: usageDisplay.note, href: usageHref, link: 'Open Codex Usage', repositoryName, live: true, busy: usagePending }),
    deploymentSummaryItem({ label: 'Tests', value: testValue, note: testNote, facts: testFacts, kind: testKind, href: canReadTests ? '#/tests' : '', link: `Open Tests (${repositoryName})`, repositoryName }),
    deploymentSummaryItem({ label: 'Health', value: healthValue, facts: healthFacts, kind: healthKind, href: '#/health', link: `Open Health (${repositoryName})`, repositoryName }),
    deploymentSummaryItem({ label: 'Latest decision', value: decisionValue, note: decisionNote, href: decisionsHref, link: `Open Decisions (${repositoryName})`, repositoryName }),
  ].join('');
  const titleId = `deployment-repository-${index}`;
  const bodyId = `deployment-repository-body-${index}`;
  const collapseKey = repositoryId || `unattributed:${deployments[0]?.deployment_id || index}`;
  const collapsed = state.collapsedDeploymentRepositories.has(collapseKey);
  const workersCollapsed = state.collapsedDeploymentWorkers.has(collapseKey);
  const count = deployments.length;
  const workersId = `deployment-workers-${index}`;
  return `<section class="deployment-repository${collapsed ? ' collapsed' : ''}" data-repository-id="${esc(repositoryId)}" aria-labelledby="${titleId}">
    <header class="deployment-repository-head">
      <h2 id="${titleId}">${esc(repositoryName)}</h2>
      ${repositoryId ? `<span class="muted mono">${esc(repositoryId)}</span>` : ''}
      <span class="deployment-repository-count">${count} ${count === 1 ? 'deployment' : 'deployments'}</span>
      <strong class="deployment-repository-status ${overall.kind}">${esc(overall.text)}</strong>
      <button class="deployment-collapse-toggle deployment-repository-toggle" type="button" data-deployment-repository-toggle="${esc(collapseKey)}" data-ui-continuation-anchor aria-expanded="${!collapsed}" aria-controls="${bodyId}" aria-label="${collapsed ? 'Expand' : 'Collapse'} ${esc(repositoryName)} repository">${planIcon(collapsed ? 'chevron-right' : 'chevron-down')}</button>
    </header>
    <div class="deployment-repository-body" id="${bodyId}"${collapsed ? ' hidden' : ''}><div class="deployment-repository-summary" aria-label="${esc(`${repositoryName} repository summary`)}">${summary}</div>
    <section class="deployment-workers${workersCollapsed ? ' collapsed' : ''}" aria-labelledby="${workersId}-title"><header class="deployment-workers-head"><div class="deployment-workers-title"><strong id="${workersId}-title">Workers</strong><span>${count}</span></div><button class="deployment-collapse-toggle deployment-workers-toggle" type="button" data-deployment-workers-toggle="${esc(collapseKey)}" data-ui-continuation-anchor aria-expanded="${!workersCollapsed}" aria-controls="${workersId}" aria-label="${workersCollapsed ? 'Expand' : 'Collapse'} ${count} ${count === 1 ? 'worker' : 'workers'} for ${esc(repositoryName)}"><span class="deployment-workers-toggle-label">${workersCollapsed ? 'Expand workers' : 'Collapse workers'}</span>${planIcon(workersCollapsed ? 'chevron-right' : 'chevron-down')}</button></header>
    <div class="deployment-records" id="${workersId}"${workersCollapsed ? ' hidden' : ''}>${deployments.map((deployment) => deploymentRecord(deployment, admin)).join('')}</div></section></div>
  </section>`;
}

function bindDeploymentCollapsibles(root) {
  root.querySelectorAll('[data-deployment-repository-toggle]').forEach((button) => button.addEventListener('click', () => {
    const key = button.dataset.deploymentRepositoryToggle;
    const collapsed = !state.collapsedDeploymentRepositories.has(key);
    if (collapsed) state.collapsedDeploymentRepositories.add(key); else state.collapsedDeploymentRepositories.delete(key);
    const section = button.closest('.deployment-repository');
    const body = document.getElementById(button.getAttribute('aria-controls'));
    section?.classList.toggle('collapsed', collapsed); if (body) body.hidden = collapsed;
    button.setAttribute('aria-expanded', String(!collapsed));
    button.setAttribute('aria-label', `${collapsed ? 'Expand' : 'Collapse'} ${section?.querySelector('h2')?.textContent || 'repository'} repository`);
    button.innerHTML = planIcon(collapsed ? 'chevron-right' : 'chevron-down');
  }));
  root.querySelectorAll('[data-deployment-workers-toggle]').forEach((button) => button.addEventListener('click', () => {
    const key = button.dataset.deploymentWorkersToggle;
    const collapsed = !state.collapsedDeploymentWorkers.has(key);
    if (collapsed) state.collapsedDeploymentWorkers.add(key); else state.collapsedDeploymentWorkers.delete(key);
    const workers = button.closest('.deployment-workers');
    const records = document.getElementById(button.getAttribute('aria-controls'));
    workers?.classList.toggle('collapsed', collapsed); if (records) records.hidden = collapsed;
    button.setAttribute('aria-expanded', String(!collapsed));
    const count = records?.querySelectorAll('.deployment-record').length || 0;
    const repositoryName = button.closest('.deployment-repository')?.querySelector('h2')?.textContent || 'repository';
    button.setAttribute('aria-label', `${collapsed ? 'Expand' : 'Collapse'} ${count} ${count === 1 ? 'worker' : 'workers'} for ${repositoryName}`);
    button.innerHTML = `<span class="deployment-workers-toggle-label">${collapsed ? 'Expand workers' : 'Collapse workers'}</span>${planIcon(collapsed ? 'chevron-right' : 'chevron-down')}`;
  }));
}

const viewDeployments = guard(async () => {
  main.innerHTML = `<section class="deployments-dashboard">${pageHeading('Deployments', '#/deployments')}${skeleton(8)}</section>`;
  if (workspace.active) {
    const result = await api('deployment.list', {});
    const deployments = result.deployments.filter(workspace.matches);
    main.innerHTML = `<section class="deployments-dashboard">${pageHeading('Deployments', '#/deployments')}${deployments.length ? `<div class="deployment-records">${deployments.map((deployment) => deploymentRecord(deployment, state.who?.administrator)).join('')}</div>` : stateBlock('empty', 'No deployments have been applied for this repository.')}</section>`;
    bind(main);
    bindDomainButtons(main, deployments);
    return;
  }
  const [deploymentResult, planResult, progressResult, usageResult, testsResult, healthResult] = await Promise.all([
    api('deployment.list', {}),
    optionalDashboardRead('plan.overview', {}),
    optionalDashboardRead('progress.repositories', {}),
    optionalDashboardRead('usage.repositories', { range: '24h' }),
    optionalDashboardRead('test.list', {}),
    optionalDashboardRead('health.repositories', {}),
  ]);
  const { deployments } = deploymentResult;
  if (!deployments.length) { main.innerHTML = `${pageHeading('Deployments', '#/deployments')}${stateBlock('empty', 'No deployments have been applied yet.')}`; return; }
  const admin = state.who?.administrator;
  const groups = new Map();
  for (const d of deployments) {
    const key = d.repository_id || `unattributed:${d.deployment_id}`;
    if (!groups.has(key)) groups.set(key, {
      repositoryId: d.repository_id || '', repositoryName: d.repository_name || 'Unattributed', deployments: [],
    });
    groups.get(key).deployments.push(d);
  }
  const orderedGroups = [...groups.values()].sort((left, right) => left.repositoryName.localeCompare(right.repositoryName));
  const decisionEntries = await Promise.all(orderedGroups.map(async (group) => [group.repositoryId,
    group.repositoryId ? await optionalDashboardRead('decision.tail', { repository_id: group.repositoryId, n: 1 }) : { value: null }]));
  const sources = {
    plan: rowsByRepository(planResult),
    progress: rowsByRepository(progressResult),
    usage: rowsByRepository(usageResult),
    tests: testsResult,
    health: rowsByRepository(healthResult),
    healthResult,
  };
  const decisions = new Map(decisionEntries);
  main.innerHTML = `<section class="deployments-dashboard">${pageHeading('Deployments', '#/deployments')}<div class="deployment-repositories">${orderedGroups.map((group, index) => repositoryDashboardSection(group, sources, decisions, admin, index)).join('')}</div></section>`;
  bind(main);
  bindDeploymentCollapsibles(main);
  bindDomainButtons(main, deployments);
  void hydratePendingDeploymentUsage(orderedGroups, sources.usage);
});

// Pop-up domain editor, shared by the list rows (✎) and the detail page.
async function openDomainDialog(d) {
  document.getElementById('domain-dialog')?.remove();
  const needsTarget = d.observed_only && !d.route_port;
  let componentOptions = '';
  if (needsTarget) {
    let components = d.components;
    if (!components) {
      try { components = (await api('deployment.status', { deployment_id: d.deployment_id })).components; }
      catch (e) { toast(e.message, 'bad'); components = []; }
    }
    componentOptions = (components || []).map((c) => `<option>${esc(c.name)}</option>`).join('');
  }
  const dlg = document.createElement('dialog');
  dlg.id = 'domain-dialog';
  dlg.innerHTML = `<h2>Domain: ${esc(d.name)}@${esc(d.source)}</h2>
    ${d.repository_name ? `<p class="muted">${esc(d.repository_name)}</p>` : ''}
    <form id="domain-form" class="inline">
      <label class="f">domain label<input name="domain" value="${esc(d.domain || '')}" placeholder="my-app" pattern="[a-z0-9]([a-z0-9-]*[a-z0-9])?" title="lowercase DNS label" autofocus></label>
      ${needsTarget ? `<label class="f">host port<input name="port" type="number" min="1" max="65535" required></label><label class="f">component<select name="component">${componentOptions}</select></label>` : ''}
      <label class="f">public (no sign-in)<input type="checkbox" name="public" ${d.public ? 'checked' : ''}></label>
      <div class="actions" style="flex-basis:100%">
        <button class="btn" type="submit">Save domain</button>
        ${d.domain ? '<button class="btn" type="button" id="domain-clear">Remove routed domain</button>' : ''}
        <button class="btn" type="button" id="domain-cancel">Cancel</button>
      </div>
    </form>`;
  document.body.appendChild(dlg);
  dlg.addEventListener('close', () => dlg.remove());
  $('#domain-cancel', dlg).addEventListener('click', () => dlg.close());
  $('#domain-form', dlg).addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const fd = new FormData(ev.target);
    const args = { deployment_id: d.deployment_id, domain: fd.get('domain') || null, public: fd.get('public') === 'on' };
    if (fd.get('port')) { args.port = Number(fd.get('port')); if (fd.get('component')) args.component = fd.get('component'); }
    await act(ev.target.querySelector('button[type=submit]'), 'deployment.set_domain', args, () => { dlg.close(); return render(); });
  });
  $('#domain-clear', dlg)?.addEventListener('click', async (ev) => {
    await act(ev.target, 'deployment.set_domain', { deployment_id: d.deployment_id, domain: null }, () => { dlg.close(); return render(); });
  });
  dlg.showModal();
}
function bindDomainButtons(root, deployments) {
  root.querySelectorAll('[data-edit-domain]').forEach((btn) => btn.addEventListener('click', () => {
    const d = deployments.find((x) => x.deployment_id === btn.dataset.editDomain);
    if (d) openDomainDialog(d);
  }));
}

const viewDeployment = guard(async (id) => {
  main.innerHTML = `${pageHeading('Deployments', '#/deployments')}${skeleton()}`;
  const d = await api('deployment.status', { deployment_id: id });
  const admin = state.who?.administrator;
  const obs = !!d.observed_only;
  const controllable = (c) => (obs ? c.binding?.kind === 'observed-container' : (c.owned && c.independent_control));
  const rows = d.components.flatMap((c) => {
    const componentRow = `<tr>
      <td class="wrap"><strong>${esc(c.name)}</strong>${c.display_name ? `<div class="muted">${esc(c.display_name)}</div>` : ''}<div class="muted">${esc(c.type)}${obs ? ' · exact recorded container' : (c.owned ? '' : ' · external')}</div></td>
      <td>${badge(c.state)} ${badge(c.health)}</td><td>${c.generation ?? '—'}</td><td>${c.port ?? '—'}</td><td>${c.restarts ?? '—'}</td>
      <td class="wrap mono">${esc(c.binding?.kind || '')} ${esc((c.binding?.identity || '').slice(0, 24))}</td>
      <td class="wrap">${c.last_error ? `<span class="badge bad">${esc(c.last_error)}</span>` : ''}</td>
      <td class="actions">${controllable(c) ? lifecycleButtons(id, c.name, 'btn btn-small', d.state) : ''}
        ${c.owned || obs ? `<button class="btn btn-small" data-logs="${esc(c.name)}">logs</button>` : ''}</td></tr>`;
    const serviceRows = (c.services || []).map((service) => `<tr class="service-row" data-compose-service="${esc(`${c.name}/${service.name}`)}">
      <td class="wrap"><strong>↳ ${esc(service.name)}</strong><div class="muted">Compose ${esc(service.role)} service</div></td>
      <td>${badge(service.state)}</td><td>${c.generation ?? '—'}</td><td>—</td><td>—</td>
      <td class="muted">${service.containers ?? 0} container${service.containers === 1 ? '' : 's'}</td><td>—</td>
      <td class="actions">${service.independent ? lifecycleButtons(id, `${c.name}/${service.name}`, 'btn btn-small', d.state) : ''}</td></tr>`);
    return [componentRow, ...serviceRows];
  }).join('');
  main.innerHTML = `${pageHeading('Deployments', '#/deployments', `${d.name}@${d.source}`, ` ${badge(d.state)} ${d.health && d.health !== d.state ? badge(d.health) : ''}`)}
    <p class="muted">${d.repository_name ? `Repository: <strong>${esc(d.repository_name)}</strong> ` : ''}<span class="mono">${esc(d.repository_id || '')}</span></p>
    <div class="grid"><div class="tile"><div class="k">Domain ${admin ? '<button class="btn btn-small" id="edit-domain">edit</button>' : ''}</div><div class="v">${d.domain ? esc(d.domain) : '—'}</div>${d.public ? '<div class="muted">public (no sign-in)</div>' : ''}</div><div class="tile"><div class="k">Route port</div><div class="v">${d.route_port ?? '—'}</div></div><div class="tile"><div class="k">Generation</div><div class="v">${d.current_generation ?? '—'}${d.previous_generation ? ` <span class="muted">(prev ${d.previous_generation})</span>` : ''}</div></div><div class="tile"><div class="k">Expires</div><div class="v">${d.ttl_expires_at ? esc(d.ttl_expires_at) : 'never'}</div></div></div>
    ${obs ? '<p class="notice muted">Imported from the live host. Start, stop, restart, and logs act on the exact recorded containers. Configuration changes (apply, rollback, remove) require adopting the stack through repository configuration.</p>' : ''}
    ${d.state === 'applying' ? '<p class="notice">Apply is still running. Closing this page does not cancel it. Refresh status after it finishes; other lifecycle actions stay unavailable meanwhile.</p>' : ''}
    <div class="actions" style="margin:12px 0">${lifecycleButtons(id, null, 'btn', d.state)}
      ${!obs && admin ? `<button class="btn" data-cmd="deployment.apply" data-args='${esc(JSON.stringify({ deployment_id: id }))}'${d.state === 'applying' ? ' disabled aria-disabled="true" title="Apply already in progress"' : ''}>apply</button><button class="btn" data-cmd="deployment.rollback" data-args='${esc(JSON.stringify({ deployment_id: id }))}'${d.state === 'applying' ? ' disabled aria-disabled="true" title="Apply in progress"' : ''}>rollback</button><button class="btn btn-danger" data-cmd="deployment.remove" data-args='${esc(JSON.stringify({ deployment_id: id, delete_data: false }))}'${d.state === 'applying' ? ' disabled aria-disabled="true" title="Apply in progress"' : ''}>Remove deployment — keep data</button><button class="btn btn-danger" data-cmd="deployment.remove" data-args='${esc(JSON.stringify({ deployment_id: id, delete_data: true }))}'${d.state === 'applying' ? ' disabled aria-disabled="true" title="Apply in progress"' : ''}>Remove deployment and delete data</button>` : ''}</div>
    <h2>Components</h2><div class="tablewrap"><table><thead><tr><th>Component</th><th>State</th><th>Gen</th><th>Port</th><th>Restarts</th><th>Binding</th><th>Error</th><th>Actions</th></tr></thead><tbody>${rows}</tbody></table></div>
    <div id="logs"></div><h2>Usage ${seg(Object.keys(RANGES), state.usageRange, 'usage-range')}</h2><div id="usage">${skeleton(2)}</div>`;
  bind(main);
  $('#edit-domain')?.addEventListener('click', () => openDomainDialog(d));
  bindSeg(main, 'usage-range', (r) => { state.usageRange = r; render(); });
  main.querySelectorAll('[data-logs]').forEach((btn) => btn.addEventListener('click', async () => {
    btn.disabled = true;
    try { const r = await api('deployment.logs', { deployment_id: id, component: btn.dataset.logs, tail_lines: 200 }, false); $('#logs').innerHTML = `<h2>Logs: ${esc(btn.dataset.logs)}</h2><h3>Untrusted log text</h3><pre class="log" aria-label="Untrusted log text">${esc(r.tail || '(empty)')}</pre>${r.log_path ? `<p class="muted mono">${esc(r.log_path)}</p>` : ''}`; }
    catch (e) { toast(e.message, 'bad'); } finally { btn.disabled = false; }
  }));
  try {
    const subjects = d.components
      .map((c) => (c.binding?.kind === 'observed-container'
        ? { name: c.name, kind: 'container', sid: c.binding.identity }
        : (c.owned ? { name: c.name, kind: 'component', sid: `${id}/${c.name}` } : null)))
      .filter(Boolean);
    const usage = await Promise.all(subjects.map(async (s) => {
      const [cpu, mem] = await Promise.all([
        metricHistory(s.kind, s.sid, 'cpu_percent', state.usageRange),
        metricHistory(s.kind, s.sid, 'memory_bytes', state.usageRange)]);
      return `<div class="chartpair"><h3>${esc(s.name)}</h3>${chart(cpu.points, pct, 'CPU')}${chart(mem.points, bytes, 'Memory')}</div>`;
    }));
    $('#usage').innerHTML = usage.length ? usage.join('') : '<p class="muted">No measured components.</p>';
  } catch (e) {
    if (e.code === 'stale') return;
    const el = $('#usage');
    if (el) el.innerHTML = stateBlock(e.code === 'permission_denied' ? 'denied' : 'error', e.message);
  }
});

// --- Tests ---------------------------------------------------------------
const TEST_TIERS = ['development', 'pre-merge', 'release'];
function testTierLabel(tier) {
  return { development: 'Development', 'pre-merge': 'Pre-merge', release: 'Release' }[tier] || 'Unavailable';
}
function testCapacityAdjustment(adjustment) {
  if (!adjustment) return '<p class="muted">No adjustment recorded.</p>';
  const reason = {
    underused_saturated_epoch: 'Increased after a saturated, underused epoch',
    sustained_pressure: 'Decreased after sustained host pressure',
    administrator_cap_changed: 'Administrator maximum changed',
  }[adjustment.reason] || healthLabel(adjustment.reason, {});
  return `<dl class="test-capacity-adjustment">
    <div><dt>Reason</dt><dd>${esc(reason)}</dd></div>
    <div><dt>Change</dt><dd>${esc(adjustment.previous_capacity)} → ${esc(adjustment.new_capacity)}</dd></div>
    <div><dt>CPU p95</dt><dd>${pct(adjustment.p95_cpu_percent)}</dd></div>
    <div><dt>Memory p95</dt><dd>${pct(adjustment.p95_memory_percent)}</dd></div>
    <div><dt>Saturated</dt><dd>${adjustment.saturation_fraction == null ? '—' : pct(Number(adjustment.saturation_fraction) * 100)}</dd></div>
    <div><dt>Epoch</dt><dd>${adjustment.epoch_seconds == null ? '—' : `${esc(adjustment.epoch_seconds)}s`}</dd></div>
    <div><dt>Recorded</dt><dd>${adjustment.at ? esc(ago(adjustment.at)) : '—'}</dd></div>
  </dl>`;
}
function openTestCapacityDialog(capacity, opener) {
  document.getElementById('test-capacity-dialog')?.remove();
  if (opener?.closest('details')) opener.closest('details').open = false;
  const dlg = document.createElement('dialog');
  dlg.id = 'test-capacity-dialog';
  dlg.innerHTML = `<div class="dialog-head"><h2>Test capacity</h2><button class="dialog-close" type="button" aria-label="Close capacity settings">×</button></div>
    <dl class="test-capacity-facts">
      <div><dt>Auto capacity</dt><dd>${esc(capacity.learned_capacity)}</dd></div>
      <div><dt>Effective</dt><dd>${esc(capacity.effective_capacity)}</dd></div>
      <div><dt>Maximum</dt><dd>${capacity.cap == null ? 'None' : esc(capacity.cap)}</dd></div>
      <div><dt>Active</dt><dd>${esc(capacity.active)}</dd></div>
      <div><dt>Waiting</dt><dd>${esc(capacity.waiting)}</dd></div>
      <div><dt>Admission</dt><dd>${capacity.paused ? badge('paused', 'warn') : badge('open', 'ok')}</dd></div>
    </dl>
    <h3>Last adjustment</h3>${testCapacityAdjustment(capacity.last_adjustment)}
    <form id="test-capacity-form" class="dialog-form">
      <label class="f">Maximum parallel checks<input name="cap" type="number" min="1" step="1" inputmode="numeric" value="${capacity.cap == null ? '' : esc(capacity.cap)}" placeholder="No maximum"></label>
      <div class="dialog-actions"><button class="btn" type="button" id="test-capacity-cancel">Cancel</button>${capacity.cap == null ? '' : '<button class="btn" type="button" id="test-capacity-clear">Clear maximum</button>'}<button class="btn btn-primary" type="submit">Save maximum</button></div>
    </form>`;
  document.body.appendChild(dlg);
  const close = () => { dlg.close(); dlg.remove(); restoreTestSettingsFocus('test-capacity-open'); };
  const saveAndReturn = async () => {
    dlg.close(); dlg.remove(); await render(); restoreTestSettingsFocus('test-capacity-open');
  };
  $('.dialog-close', dlg).addEventListener('click', close);
  $('#test-capacity-cancel', dlg).addEventListener('click', close);
  dlg.addEventListener('cancel', (event) => { event.preventDefault(); close(); });
  $('#test-capacity-form', dlg).addEventListener('submit', async (event) => {
    event.preventDefault();
    const input = event.target.querySelector('[name=cap]');
    input.setCustomValidity('');
    const raw = new FormData(event.target).get('cap')?.trim();
    const cap = raw ? Number(raw) : null;
    if (cap != null && (!Number.isSafeInteger(cap) || cap < 1)) {
      input.setCustomValidity('Enter a whole number of at least 1.');
      event.target.reportValidity(); return;
    }
    const button = event.submitter || event.target.querySelector('button[type=submit]');
    await act(button, 'test.capacity.set', { cap }, saveAndReturn);
  });
  $('#test-capacity-clear', dlg)?.addEventListener('click', async (event) => {
    await act(event.target, 'test.capacity.set', { cap: null }, saveAndReturn);
  });
  dlg.showModal();
  requestAnimationFrame(() => $('#test-capacity-form [name=cap]', dlg)?.focus());
}

function logSelector(entry) {
  const ref = entry.log_ref || entry;
  return ['run_id', 'check', 'phase', 'case', 'stream'].reduce((out, key) => {
    if (ref[key] != null) out[key] = ref[key];
    return out;
  }, {});
}
function logEntryLabel(entry) {
  const ref = logSelector(entry);
  const stream = ref.stream === 'stderr' ? 'Error output' : 'Standard output';
  if (ref.phase === 'executor') return `Test runner · ${stream}`;
  if (ref.phase === 'discovery') return `${ref.check} · Discovery · ${stream}`;
  if (ref.phase === 'case') return `${ref.check} · ${ref.case || 'Cases'} · ${stream}`;
  return `${ref.check || 'Check'} · ${stream}`;
}
function logResultRows(result) {
  for (const key of ['segments', 'matches', 'contexts']) {
    if (Array.isArray(result[key])) return result[key];
  }
  return [];
}
function logRowKey(row) {
  return [row.line_start, row.line_end, row.byte_start, row.byte_end, row.text, row.base64].join('\u0000');
}
function logRowAnchor(row) {
  return [row.line_start ?? 'bytes', row.line_end ?? 'bytes', row.byte_start ?? 0, row.byte_end ?? 0].join('-');
}
function mergeLogRows(current, incoming, prepend = false) {
  const ordered = prepend ? [...incoming, ...current] : [...current, ...incoming];
  const seen = new Set();
  return ordered.filter((row) => {
    const key = logRowKey(row);
    if (seen.has(key)) return false;
    seen.add(key); return true;
  });
}
function formatStructuredLogText(content) {
  const text = String(content ?? '');
  const pretty = (value, trailingNewline = false) => {
    const formatted = JSON.stringify(value, null, 2);
    return `${formatted}${trailingNewline ? '\n' : ''}`;
  };
  const trimmed = text.trim();
  if (trimmed && ['{', '['].includes(trimmed[0])) {
    try {
      return { text: pretty(JSON.parse(trimmed), text.endsWith('\n')), format: 'Formatted JSON' };
    } catch { /* try JSON lines below */ }
  }
  let jsonLines = 0;
  let previousJson = false;
  const lines = [];
  for (const line of text.split('\n')) {
    const candidate = line.trim();
    if (!candidate || !['{', '['].includes(candidate[0])) {
      lines.push(line); previousJson = false; continue;
    }
    try {
      const indent = line.slice(0, line.length - line.trimStart().length);
      if (previousJson) lines.push('');
      jsonLines += 1;
      lines.push(pretty(JSON.parse(candidate)).split('\n').map((part) => `${indent}${part}`).join('\n'));
      previousJson = true;
    } catch { lines.push(line); previousJson = false; }
  }
  return { text: lines.join('\n'), format: jsonLines ? 'Formatted JSON lines' : '' };
}
function highlightLogText(text) {
  const pattern = /"(?:\\.|[^"\\])*"|\b[A-Za-z_][A-Za-z0-9_.-]*(?=\s*[:=])|\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\b|\b(?:0x[0-9a-f]+|\d+(?:\.\d+)?(?:e[+-]?\d+)?)\b|\b(?:true|false|null|undefined)\b|\b(?:fatal|error|failed|failure|panic)\b|\b(?:warn|warning|timeout|timed-out|retry)\b|\b(?:ok|pass|passed|success|successful|running|complete|completed)\b/gi;
  let html = ''; let cursor = 0;
  for (const match of text.matchAll(pattern)) {
    const token = match[0]; const index = match.index;
    html += esc(text.slice(cursor, index));
    const after = text.slice(index + token.length);
    let kind = 'string';
    if (token.startsWith('"')) kind = /^\s*:/.test(after) ? 'key' : 'string';
    else if (/^[A-Za-z_]/.test(token) && /^\s*[:=]/.test(after)) kind = 'key';
    else if (/^\d{4}-\d{2}-\d{2}T/i.test(token)) kind = 'time';
    else if (/^(?:0x|\d)/i.test(token)) kind = 'number';
    else if (/^(?:true|false|null|undefined)$/i.test(token)) kind = 'keyword';
    else if (/^(?:fatal|error|failed|failure|panic)$/i.test(token)) kind = 'failure';
    else if (/^(?:warn|warning|timeout|timed-out|retry)$/i.test(token)) kind = 'warning';
    else if (/^(?:ok|pass|passed|success|successful|running|complete|completed)$/i.test(token)) kind = 'success';
    html += `<span class="log-token log-token-${kind}">${esc(token)}</span>`;
    cursor = index + token.length;
  }
  return `${html}${esc(text.slice(cursor))}`;
}
function renderLogRows(rows, emptyCopy = 'No log output yet.') {
  if (!rows.length) return stateBlock('empty', emptyCopy);
  return `<div class="log-results">${rows.map((row) => {
    const coordinates = row.line_start != null
      ? `Lines ${row.line_start}${row.line_end && row.line_end !== row.line_start ? `–${row.line_end}` : ''}`
      : `Bytes ${row.byte_start ?? 0}–${row.byte_end ?? 0}`;
    const count = row.occurrences > 1 ? ` · ${row.occurrences} occurrences` : '';
    const content = row.text != null ? row.text : (row.base64 != null ? `base64:${row.base64}` : '');
    const formatted = formatStructuredLogText(content);
    return `<section class="log-result${formatted.format ? ' structured' : ''}" data-log-row-anchor="${esc(logRowAnchor(row))}"><h3>${esc(coordinates)}${esc(count)}${formatted.format ? `<span class="log-format-badge">${esc(formatted.format)}</span>` : ''}</h3><pre class="log" aria-label="Untrusted log text"><code>${highlightLogText(formatted.text)}</code></pre></section>`;
  }).join('')}</div>`;
}
function renderLogError(message) {
  return `<div class="notice"><strong>Could not load log.</strong> ${esc(message)}</div>`;
}
async function openTestLogsDialog(run, retention, opener) {
  document.getElementById('test-logs-dialog')?.remove();
  const dlg = document.createElement('dialog');
  dlg.id = 'test-logs-dialog';
  dlg.innerHTML = `<div class="dialog-head"><h2>Test logs</h2><button class="dialog-close" type="button" aria-label="Close test logs">×</button></div>
    <div id="test-log-catalog">${skeleton()}</div>`;
  document.body.appendChild(dlg);
  const close = () => {
    dlg.close(); dlg.remove();
    const returnTarget = opener?.isConnected ? opener
      : document.querySelector(`[data-test-logs][data-run-id="${CSS.escape(run.run_id)}"]`);
    returnTarget?.focus();
  };
  $('.dialog-close', dlg).addEventListener('click', close);
  dlg.addEventListener('cancel', (event) => { event.preventDefault(); close(); });
  dlg.showModal();

  const catalogRoot = $('#test-log-catalog', dlg);
  let entries = [];
  let catalogCursor = null;
  let selectedIndex = 0;
  let readVersion = 0;
  let paging = false;
  let pagingPaused = false;
  let adjustingScroll = false;
  let reader = { operation: 'tail', options: {}, rows: [], cursor: null, atLatest: true };
  const catalogArgs = () => ({ path: run.worktree_path, run_id: run.run_id, limit: 100,
    ...(catalogCursor ? { cursor: catalogCursor } : {}) });
  const selectedEntry = () => entries[Number($('#test-log-stream', dlg)?.value ?? selectedIndex)];
  const logSummary = (entry) => {
    const lines = entry.lines == null ? 'Line count pending' : `${Number(entry.lines).toLocaleString()} lines`;
    const stateCopy = entry.complete ? 'Complete' : 'In progress';
    const expiry = entry.expires_at ? `Retained ${until(entry.expires_at)}` : 'Active';
    return `${lines} · ${bytes(entry.bytes)} · ${stateCopy} · ${expiry}`;
  };
  const readerTitle = () => reader.operation === 'search' ? 'Search results'
    : reader.operation === 'failure_context' ? 'Likely failure' : 'Latest output';
  const readerStatus = () => {
    if (!reader.rows.length) return '';
    const lineRows = reader.rows.filter((row) => row.line_start != null);
    if (reader.operation === 'tail' && lineRows.length) {
      const start = Math.min(...lineRows.map((row) => row.line_start));
      const end = Math.max(...lineRows.map((row) => row.line_end ?? row.line_start));
      return `Showing lines ${start}–${end}`;
    }
    return `${reader.rows.length} ${reader.rows.length === 1 ? 'excerpt' : 'excerpts'}`;
  };
  const syncReaderActions = () => {
    const entry = selectedEntry();
    const latest = $('#test-log-latest', dlg);
    if (!entry || !latest) return;
    latest.textContent = entry.complete ? 'Jump to latest' : 'Refresh latest';
    latest.hidden = reader.atLatest && entry.complete;
    latest.closest('.test-log-toolbar')?.classList.toggle('show-latest', !latest.hidden);
  };
  const renderReader = (scroll = 'top', scrollAnchor = null) => {
    const target = $('#test-log-read-result', dlg);
    if (!target) return;
    adjustingScroll = true;
    const emptyCopy = reader.operation === 'search' ? 'No matching log lines.'
      : reader.operation === 'failure_context' ? 'No likely failure was found in this stream.' : 'No log output yet.';
    const boundary = reader.rows.length && !reader.cursor
      ? `<div class="log-boundary">${reader.operation === 'tail' ? 'Start of output' : 'All results shown'}</div>` : '';
    target.innerHTML = reader.operation === 'tail'
      ? `${boundary}${renderLogRows(reader.rows, emptyCopy)}`
      : `${renderLogRows(reader.rows, emptyCopy)}${boundary}`;
    target.setAttribute('aria-busy', 'false');
    $('#test-log-view-title', dlg).textContent = readerTitle();
    $('#test-log-view-status', dlg).textContent = readerStatus();
    syncReaderActions();
    requestAnimationFrame(() => {
      if (scroll === 'bottom') target.scrollTop = target.scrollHeight;
      else if (scroll === 'prepend' && scrollAnchor) {
        const anchor = target.querySelector(`[data-log-row-anchor="${CSS.escape(scrollAnchor.id)}"]`);
        if (anchor) {
          const offset = anchor.getBoundingClientRect().top - target.getBoundingClientRect().top;
          target.scrollTop = Math.max(0, offset - scrollAnchor.offset);
        }
      }
      else if (scroll === 'top') target.scrollTop = 0;
      requestAnimationFrame(() => {
        adjustingScroll = false;
        if (reader.cursor && target.scrollHeight <= target.clientHeight + 1) maybeLoadMore();
      });
    });
  };
  const maybeLoadMore = () => {
    const target = $('#test-log-read-result', dlg);
    if (!target || paging || pagingPaused || adjustingScroll || !reader.cursor || target.getAttribute('aria-busy') === 'true') return;
    const nearBoundary = reader.operation === 'tail'
      ? target.scrollTop <= 56
      : target.scrollHeight - target.scrollTop - target.clientHeight <= 56;
    if (!nearBoundary) return;
    read(reader.operation, reader.options, reader.cursor,
      reader.operation === 'tail' ? 'prepend' : 'append');
  };
  const read = async (operation, options = {}, cursor = null, direction = 'replace') => {
    const entry = selectedEntry();
    if (!entry) return;
    if (direction !== 'replace' && paging) return;
    if (direction === 'replace') { paging = false; pagingPaused = false; }
    else { paging = true; pagingPaused = false; }
    const version = ++readVersion;
    const target = $('#test-log-read-result', dlg);
    const priorRows = reader.rows;
    const anchorRow = direction === 'prepend' ? priorRows[0] : null;
    const anchorElement = anchorRow ? target?.querySelector(`[data-log-row-anchor="${CSS.escape(logRowAnchor(anchorRow))}"]`) : null;
    const scrollAnchor = anchorElement ? {
      id: logRowAnchor(anchorRow),
      offset: anchorElement.getBoundingClientRect().top - target.getBoundingClientRect().top,
    } : null;
    if (direction === 'replace') {
      target.innerHTML = skeleton();
      $('#test-log-view-title', dlg).textContent = operation === 'search' ? 'Searching log'
        : operation === 'failure_context' ? 'Finding likely failure' : 'Loading latest output';
      $('#test-log-view-status', dlg).textContent = '';
    } else {
      target.querySelector('.log-page-error')?.remove();
      const indicator = document.createElement('div');
      indicator.className = 'log-loading-more'; indicator.id = 'test-log-loading-more';
      indicator.textContent = operation === 'tail' ? 'Loading earlier output…' : 'Loading more results…';
      if (operation === 'tail') target.prepend(indicator); else target.append(indicator);
      $('#test-log-view-status', dlg).textContent = indicator.textContent;
    }
    target.setAttribute('aria-busy', 'true');
    try {
      const result = await api(`test.log.${operation}`, {
        path: run.worktree_path, ...logSelector(entry), ...options, ...(cursor ? { cursor } : {}),
      }, false);
      if (version !== readVersion || !dlg.isConnected) return;
      paging = false;
      const incoming = logResultRows(result);
      const rows = direction === 'prepend' ? mergeLogRows(priorRows, incoming, true)
        : direction === 'append' ? mergeLogRows(priorRows, incoming) : incoming;
      reader = {
        operation, options, rows, cursor: result.next_cursor || null,
        atLatest: operation === 'tail' && cursor == null,
      };
      renderReader(direction === 'prepend' ? 'prepend' : operation === 'tail' ? 'bottom' : 'top', scrollAnchor);
    } catch (error) {
      if (version !== readVersion || !dlg.isConnected) return;
      paging = false;
      target.setAttribute('aria-busy', 'false');
      if (direction === 'replace') {
        target.innerHTML = `${renderLogError(error.message)}<button class="btn" type="button" id="test-log-retry">Try again</button>`;
        $('#test-log-view-title', dlg).textContent = 'Log unavailable';
        $('#test-log-view-status', dlg).textContent = '';
        $('#test-log-retry', target).addEventListener('click', () => read(operation, options, cursor, direction));
      } else {
        pagingPaused = true;
        target.querySelector('#test-log-loading-more')?.remove();
        const errorBox = document.createElement('div'); errorBox.className = 'log-page-error';
        errorBox.innerHTML = `<span>${esc(error.message)}</span><button class="btn btn-small" type="button">Try again</button>`;
        if (operation === 'tail') target.prepend(errorBox); else target.append(errorBox);
        $('#test-log-view-status', dlg).textContent = 'More output could not be loaded';
        $('button', errorBox).addEventListener('click', () => {
          pagingPaused = false; read(operation, options, cursor, direction);
        });
      }
    }
  };
  const readLatest = () => read('tail', { lines: 200, max_bytes: 49152 });
  const renderCatalogue = () => {
    if (!entries.length) {
      catalogRoot.innerHTML = stateBlock('empty', 'No retained logs for this run.'); return;
    }
    selectedIndex = Math.min(selectedIndex, entries.length - 1);
    catalogRoot.innerHTML = `<div class="test-log-reader">
      <div class="test-log-stream-row"><label class="f">Output stream<select id="test-log-stream">${entries.map((entry, index) => `<option value="${index}"${index === selectedIndex ? ' selected' : ''}>${esc(logEntryLabel(entry))}</option>`).join('')}</select></label>
        ${catalogCursor ? '<button class="btn" type="button" id="test-log-more">Show more streams</button>' : ''}</div>
      <div class="test-log-summary" id="test-log-summary"></div>
      <details class="test-log-details"><summary>Stream details</summary><div id="test-log-metadata"></div></details>
      <div class="test-log-toolbar">
        <form id="test-log-search" class="test-log-search"><label class="sr-only" for="test-log-search-text">Search this log</label><input id="test-log-search-text" name="text" type="search" required maxlength="4096" placeholder="Search this log"><button class="btn" type="submit">Search</button></form>
        <button class="btn" type="button" data-log-read="failure_context">Show likely failure</button>
        <button class="btn" type="button" id="test-log-latest" hidden>Jump to latest</button>
      </div>
      <section class="test-log-viewer" aria-labelledby="test-log-view-title"><div class="test-log-view-head"><div><strong id="test-log-view-title" data-ui-continuation-anchor>Loading latest output</strong><span id="test-log-view-status"></span></div><span class="test-log-trust">Untrusted log output</span></div>
        <div id="test-log-read-result" class="test-log-scroll" tabindex="0" aria-live="polite" aria-busy="true">${skeleton()}</div></section>
      </div>`;
    const metadata = () => {
      const entry = selectedEntry();
      $('#test-log-summary', dlg).textContent = logSummary(entry);
      $('#test-log-metadata', dlg).innerHTML = `<dl class="test-log-facts">
        <div><dt>Bytes</dt><dd>${bytes(entry.bytes)}</dd></div><div><dt>Lines</dt><dd>${entry.lines == null ? 'Pending' : esc(entry.lines)}</dd></div>
        <div><dt>Complete</dt><dd>${entry.complete ? 'Yes' : 'In progress'}</dd></div><div><dt>Truncated</dt><dd>${entry.truncated ? 'Yes' : 'No'}</dd></div>
        <div><dt>First output</dt><dd>${entry.first_byte_at ? esc(ago(entry.first_byte_at)) : '—'}</dd></div><div><dt>Last output</dt><dd>${entry.last_byte_at ? esc(ago(entry.last_byte_at)) : '—'}</dd></div>
        <div><dt>SHA-256</dt><dd class="mono">${entry.sha256 ? esc(entry.sha256) : 'Pending'}</dd></div><div><dt>Expires</dt><dd>${entry.expires_at ? esc(until(entry.expires_at)) : 'Active'}</dd></div>
        <div><dt>History depth</dt><dd>${entry.depth_rank == null ? 'Active' : `${esc(entry.depth_rank)} of ${esc(retention.case_depth)}`}</dd></div>
        <div><dt>Structured evidence</dt><dd>${entry.structured_evidence?.available ? `${esc(entry.structured_evidence.count)} · ${esc((entry.structured_evidence.formats || []).join(', '))}` : 'None'}</dd></div>
      </dl>`;
    };
    metadata();
    $('#test-log-stream', dlg).addEventListener('change', (event) => {
      selectedIndex = Number(event.target.value); readVersion += 1; paging = false; pagingPaused = false; metadata(); readLatest();
    });
    $('[data-log-read="failure_context"]', dlg).addEventListener('click', () => read(
      'failure_context', { limit: 20, context_lines: 2, max_bytes: 32768 }));
    $('#test-log-latest', dlg).addEventListener('click', readLatest);
    $('#test-log-search', dlg).addEventListener('submit', (event) => {
      event.preventDefault();
      const text = String(new FormData(event.target).get('text') || '').trim();
      if (!text) return;
      read('search', { text, max_matches: 20, context_lines: 2, max_bytes: 32768 });
    });
    $('#test-log-read-result', dlg).addEventListener('scroll', maybeLoadMore, { passive: true });
    $('#test-log-more', dlg)?.addEventListener('click', loadCatalogue);
    readLatest();
  };
  async function loadCatalogue() {
    try {
      const result = await api('test.log.catalog', catalogArgs(), false);
      entries = entries.concat(result.entries || []); catalogCursor = result.next_cursor || null;
      renderCatalogue();
    } catch (error) {
      if (entries.length) { toast(error.message, 'bad'); return; }
      catalogRoot.innerHTML = `${renderLogError(error.message)}<button class="btn" type="button" id="test-log-catalog-retry">Try again</button>`;
      $('#test-log-catalog-retry', catalogRoot).addEventListener('click', loadCatalogue);
    }
  }
  await loadCatalogue();
  requestAnimationFrame(() => $('#test-log-stream', dlg)?.focus());
}

function openTestLogRetentionDialog(retention, opener) {
  document.getElementById('test-log-retention-dialog')?.remove();
  if (opener?.closest('details')) opener.closest('details').open = false;
  const dlg = document.createElement('dialog'); dlg.id = 'test-log-retention-dialog';
  const hours = retention.max_age_seconds / 3600;
  dlg.innerHTML = `<div class="dialog-head"><h2>Log retention</h2><button class="dialog-close" type="button" aria-label="Close log retention settings">×</button></div>
    <form id="test-log-retention-form" class="dialog-form">
      <label class="f">Maximum age in hours<input name="max_age_hours" autocomplete="off" type="number" min="0.0002777778" step="any" required value="${esc(hours)}"></label>
      <label class="f">Runs kept per case<input name="case_depth" autocomplete="off" type="number" min="1" max="65535" step="1" required value="${esc(retention.case_depth)}"></label>
      <div class="dialog-actions"><button class="btn" type="button" data-retention-cancel>Cancel</button><button class="btn btn-primary" type="submit">Save and clean eligible logs</button></div>
    </form>`;
  document.body.appendChild(dlg);
  const close = () => { dlg.close(); dlg.remove(); restoreTestSettingsFocus('test-log-retention-open'); };
  $('.dialog-close', dlg).addEventListener('click', close);
  $('[data-retention-cancel]', dlg).addEventListener('click', close);
  dlg.addEventListener('cancel', (event) => { event.preventDefault(); close(); });
  $('#test-log-retention-form', dlg).addEventListener('submit', async (event) => {
    event.preventDefault(); const data = new FormData(event.target);
    const seconds = Math.round(Number(data.get('max_age_hours')) * 3600); const depth = Number(data.get('case_depth'));
    if (!Number.isSafeInteger(seconds) || seconds < 1 || seconds > 315360000 || !Number.isSafeInteger(depth) || depth < 1 || depth > 65535) {
      toast('Enter a positive age and a whole-number case depth.', 'bad'); return;
    }
    const button = event.submitter; await act(button, 'test.log.retention.set',
      { max_age_seconds: seconds, case_depth: depth }, async () => {
        dlg.close(); dlg.remove(); await render(); restoreTestSettingsFocus('test-log-retention-open');
      });
  });
  dlg.showModal(); requestAnimationFrame(() => $('[name=max_age_hours]', dlg)?.focus());
}

const EVIDENCE_TOOLS = [
  ['select', 'pointer', 'Select'],
  ['pin', 'message-plus', 'Pin comment'],
  ['rectangle', 'square', 'Rectangle'],
  ['arrow', 'arrow-right', 'Arrow'],
  ['freehand', 'pencil', 'Freehand'],
  ['highlight', 'highlight', 'Highlight'],
  ['text', 'letter-t', 'Text'],
];
const EVIDENCE_COLORS = [
  ['#4c8dff', 'Blue'], ['#f59e0b', 'Amber'], ['#ef4444', 'Red'],
  ['#22c55e', 'Green'], ['#a855f7', 'Purple'], ['#f8fafc', 'White'],
];

function resetEvidenceImages() {
  for (const url of state.evidenceImageUrls.values()) URL.revokeObjectURL(url);
  state.evidenceImageUrls.clear(); state.evidenceImagePromises.clear();
}

function resetEvidenceDraft() {
  state.evidencePendingLabel = null;
  $('.evidence-label-editor', main)?.remove();
  state.evidenceComposerDismissed = false;
  state.evidenceDrafts.delete(state.evidenceDraftKey);
  state.evidenceDraftBody = '';
  state.evidenceDraftMarks = [];
  state.evidenceUndo = [];
  state.evidenceRedo = [];
  state.evidenceSelectedMarkId = null;
  state.evidenceSelectedFeedbackId = null;
}

function evidenceMarkId() {
  return `mark-${crypto.getRandomValues(new Uint32Array(2)).join('-')}`;
}

function rememberEvidenceDraft() {
  if (!state.evidenceDraftKey) return;
  state.evidenceDrafts.set(state.evidenceDraftKey, {
    marks: cloneEvidenceMarks(state.evidenceDraftMarks), body: state.evidenceDraftBody,
    undo: state.evidenceUndo, redo: state.evidenceRedo, zoom: state.evidenceZoom, label: state.evidencePendingLabel,
  });
}

function activateEvidenceDraft(imageId) {
  const key = `${state.evidenceRunId}:${imageId || ''}`;
  if (state.evidenceDraftKey === key) return;
  rememberEvidenceDraft();
  const draft = state.evidenceDrafts.get(key);
  state.evidenceDraftKey = key;
  state.evidenceComposerDismissed = false;
  state.evidenceDraftMarks = cloneEvidenceMarks(draft?.marks || []);
  state.evidenceDraftBody = draft?.body || '';
  state.evidencePendingLabel = draft?.label || null;
  $('.evidence-label-editor', main)?.remove();
  state.evidenceUndo = draft?.undo || [];
  state.evidenceRedo = draft?.redo || [];
  state.evidenceZoom = draft?.zoom || 1;
  state.evidenceSelectedMarkId = null;
  state.evidenceSelectedFeedbackId = null;
  $('#evidence-composer', main)?.remove();
}

function revealEvidenceDiscussion() {
  evidenceLayoutSession?.showDetails();
  $('.evidence-page', main)?.classList.add('inspector-open');
  refreshEvidenceInspector();
  requestAnimationFrame(() => {
    const discussion = $('.evidence-thread', main);
    discussion?.scrollIntoView({ block: 'nearest' });
    $('textarea', discussion || main)?.focus({ preventScroll: true });
  });
}

function ensureEvidenceComposer() {
  let composer = $('#evidence-composer', main);
  if (composer) return composer;
  const page = $('.evidence-page', main);
  if (!page) return null;
  composer = document.createElement('section');
  composer.id = 'evidence-composer';
  composer.className = 'evidence-composer';
  composer.hidden = true;
  composer.setAttribute('role', 'dialog');
  composer.setAttribute('aria-labelledby', 'evidence-compose-title');
  composer.innerHTML = `<h2 id="evidence-compose-title">New feedback</h2><form id="evidence-feedback-create"><label class="f">Suggestion<textarea name="body" rows="3" minlength="3" maxlength="2000" required placeholder="Describe what should change">${esc(state.evidenceDraftBody)}</textarea></label><p id="evidence-feedback-error" role="alert" hidden></p><div class="actions"><button class="btn" type="button" data-evidence-cancel>Cancel</button><button class="btn btn-primary" type="submit" disabled>Create feedback task</button></div></form>`;
  $('#evidence-scroll', page).appendChild(composer);
  composer.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    event.preventDefault(); state.evidenceComposerDismissed = true; updateEvidenceToolbar();
    $('#evidence-canvas', main)?.focus({ preventScroll: true });
  });
  const form = $('form', composer);
  const requirement = document.createElement('p');
  requirement.id = 'evidence-feedback-requirement'; requirement.className = 'muted'; requirement.hidden = true;
  form.insertBefore(requirement, $('.actions', form));
  form.addEventListener('input', () => {
    state.evidenceDraftBody = form.elements.body.value;
    $('#evidence-feedback-error', form).hidden = true;
    updateEvidenceToolbar();
  });
  $('[data-evidence-cancel]', composer).addEventListener('click', () => {
    resetEvidenceDraft(); form.elements.body.value = '';
    updateEvidenceToolbar(); redrawEvidenceCanvas();
    $('#evidence-canvas', main)?.focus({ preventScroll: true });
  });
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const { screenshot } = currentEvidenceSelection();
    const body = state.evidenceDraftBody.trim();
    if (!screenshot || body.length < 3 || !state.evidenceDraftMarks.length || state.evidenceSavingKey || state.evidencePendingLabel) return;
    const key = state.evidenceDraftKey;
    state.evidenceSavingKey = key;
    updateEvidenceToolbar();
    let failure;
    const result = await evidenceMutation(event.submitter, 'test.evidence.feedback.create', {
      image_id: screenshot.image_id, body, marks: cloneEvidenceMarks(state.evidenceDraftMarks),
    }, error => { failure = error; });
    state.evidenceSavingKey = null;
    if (result) {
      state.evidenceDrafts.delete(key);
      if (state.evidenceDraftKey === key) {
        resetEvidenceDraft(); form.elements.body.value = '';
        state.evidenceSelectedFeedbackId = result.feedback.feedback_id;
        revealEvidenceDiscussion(); redrawEvidenceCanvas(); updateEvidencePageStatus();
      }
      toast('Feedback task created', 'ok');
    } else if (state.evidenceDraftKey === key) {
      const error = $('#evidence-feedback-error', form);
      error.textContent = `Could not confirm the save: ${failure?.message || 'the request failed'}. Your comment and marks are still here.`;
      error.hidden = false;
      openEvidenceComposer(undefined, false);
    }
    updateEvidenceToolbar();
  });
  return composer;
}

function openEvidenceComposer(point, focus = true) {
  state.evidenceComposerDismissed = false;
  const composer = ensureEvidenceComposer();
  if (!composer) return;
  composer.hidden = false;
  const scroll = $('#evidence-scroll', main);
  const imageArea = scroll.getBoundingClientRect();
  const width = Math.min(340, window.innerWidth - 24, imageArea.width - 16);
  composer.style.width = `${width}px`;
  const canvas = $('#evidence-canvas', main)?.getBoundingClientRect();
  const anchorX = canvas && point ? canvas.left + point.x * canvas.width : window.innerWidth;
  const anchorY = canvas && point ? canvas.top + point.y * canvas.height : window.innerHeight;
  const height = composer.getBoundingClientRect().height;
  const leftEdge = Math.max(12, imageArea.left + 8);
  const rightEdge = Math.min(innerWidth - 12, imageArea.right - 8);
  const topEdge = Math.max(12, imageArea.top + 8);
  const bottomEdge = Math.min(innerHeight - 12, imageArea.bottom - 8);
  const left = anchorX + width + 24 < rightEdge ? anchorX + 20 : anchorX - width - 20;
  const top = anchorY + height + 24 < bottomEdge ? anchorY + 20 : anchorY - height - 20;
  composer.style.maxHeight = `${Math.max(180, bottomEdge - topEdge)}px`;
  composer.style.left = `${Math.max(leftEdge, Math.min(rightEdge - width, left)) - imageArea.left + scroll.scrollLeft}px`;
  composer.style.top = `${Math.max(topEdge, Math.min(bottomEdge - height, top)) - imageArea.top + scroll.scrollTop}px`;
  if (focus) $('textarea', composer)?.focus({ preventScroll: true });
}

function evidenceStepLabel(value) {
  const text = String(value || 'Base state').replace(/[-_]+/g, ' ').trim();
  return text ? text.charAt(0).toUpperCase() + text.slice(1) : 'Base state';
}

function evidenceSteps(data) {
  const groups = new Map();
  for (const bundle of data.bundles || []) {
    for (const cell of bundle.cells || []) {
      const targetBase = String(cell.target_name || '').replace(/\s+\[[^\]]+\]$/, '');
      const identity = [bundle.formal_run_id, cell.primary_journey, targetBase,
        cell.state_name, cell.requested_path].join('\u0000');
      if (!groups.has(identity)) {
        groups.set(identity, {
          key: `step-${groups.size + 1}`,
          index: groups.size,
          label: evidenceStepLabel(cell.state_name),
          target: targetBase || cell.target_name,
          journey: cell.primary_journey || targetBase || 'UI journey',
          route: cell.final_path || cell.requested_path || 'Route unavailable',
          variants: [],
        });
      }
      groups.get(identity).variants.push({ ...cell, bundle });
    }
  }
  const steps = [...groups.values()];
  for (const step of steps) {
    step.variants.sort((left, right) => (right.viewport?.width || 0) - (left.viewport?.width || 0));
    step.result = step.variants.some((cell) => cell.outcome !== 'checked' || (cell.findings || []).some((finding) => finding.severity === 'critical')) ? 'failed'
      : step.variants.some((cell) => (cell.findings || []).length || (cell.review?.decision && cell.review.decision !== 'pass')) ? 'attention'
        : 'passed';
    step.duration = Math.max(...step.variants.map((cell) => cell.duration_ms || 0));
  }
  return steps;
}

function currentEvidenceSelection() {
  const step = state.evidenceSteps.find((item) => item.key === state.evidenceStepKey)
    || state.evidenceSteps[0];
  if (!step) return { step: null, cell: null, screenshot: null };
  const cell = step.variants.find((item) => item.viewport?.name === state.evidenceViewport)
    || step.variants[0];
  let kind = state.evidenceScreenshotKind;
  let screenshot = cell.screenshots?.[kind];
  if (!screenshot || screenshot.status !== 'available') {
    kind = kind === 'viewport' ? 'full_page' : 'viewport';
    screenshot = cell.screenshots?.[kind];
  }
  if (!screenshot || screenshot.status !== 'available') screenshot = null;
  state.evidenceStepKey = step.key;
  state.evidenceViewport = cell.viewport?.name || null;
  state.evidenceScreenshotKind = kind;
  return { step, cell, screenshot };
}

async function evidenceImageUrl(run, image) {
  if (!image?.image_id) throw new ApiError('test_evidence_not_found', 'Screenshot unavailable');
  if (state.evidenceImageUrls.has(image.image_id)) return state.evidenceImageUrls.get(image.image_id);
  if (state.evidenceImagePromises.has(image.image_id)) return state.evidenceImagePromises.get(image.image_id);
  const promise = (async () => {
    const chunks = []; let offset = 0; let mime = image.mime || 'image/png';
    do {
      const result = await api('test.evidence.image', {
        path: run.worktree_path, run_id: run.run_id, image_id: image.image_id,
        offset, max_bytes: 184320,
      });
      mime = result.mime || mime;
      const binary = atob(result.base64 || '');
      const bytes = new Uint8Array(binary.length);
      for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
      chunks.push(bytes);
      offset = result.next_offset;
    } while (offset != null);
    const url = URL.createObjectURL(new Blob(chunks, { type: mime }));
    state.evidenceImageUrls.set(image.image_id, url);
    state.evidenceImagePromises.delete(image.image_id);
    return url;
  })().catch((error) => {
    state.evidenceImagePromises.delete(image.image_id); throw error;
  });
  state.evidenceImagePromises.set(image.image_id, promise);
  return promise;
}

function evidenceResultMark(result) {
  if (result === 'passed') return '<span class="evidence-result ok">Passed</span>';
  if (result === 'attention') return '<span class="evidence-result warn">Review</span>';
  return '<span class="evidence-result bad">Failed</span>';
}

function renderEvidenceRail() {
  return state.evidenceSteps.map((step, index) => {
    const active = step.key === state.evidenceStepKey;
    const preview = step.variants.find((cell) => cell.screenshots?.viewport?.status === 'available')
      || step.variants.find((cell) => cell.screenshots?.full_page?.status === 'available');
    const image = preview?.screenshots?.viewport?.status === 'available'
      ? preview.screenshots.viewport : preview?.screenshots?.full_page;
    return `<button type="button" class="evidence-step${active ? ' active' : ''}" data-evidence-step="${esc(step.key)}" aria-pressed="${active}">
      <span class="evidence-step-thumb">${image ? `<img alt="" data-evidence-thumb="${esc(image.image_id)}">` : '<span>Unavailable</span>'}</span>
      <span class="evidence-step-copy"><strong><i>${index + 1}</i>${esc(step.label)}</strong><small>${esc(step.route)}</small><span>${step.variants.map((cell) => `<b>${esc(cell.viewport?.name || 'viewport')}</b>`).join('')} · ${step.duration ? `${(step.duration / 1000).toFixed(1)}s` : '—'}</span></span>
      ${evidenceResultMark(step.result)}
    </button>`;
  }).join('');
}

function renderEvidenceVariants(step, selectedCell) {
  return step.variants.map((cell) => {
    const image = cell.screenshots?.viewport?.status === 'available'
      ? cell.screenshots.viewport : cell.screenshots?.full_page;
    const active = cell === selectedCell;
    return `<button type="button" class="evidence-variant${active ? ' active' : ''}" data-evidence-viewport="${esc(cell.viewport?.name || '')}" aria-pressed="${active}">
      <span>${image ? `<img alt="${esc(`${step.label}, ${cell.viewport?.name || 'viewport'}`)}" data-evidence-thumb="${esc(image.image_id)}">` : '<span class="muted">Screenshot unavailable</span>'}</span>
      <strong>${esc(cell.viewport?.name || 'Viewport')}</strong><small>${esc(cell.viewport?.width)} × ${esc(cell.viewport?.height)}</small>
    </button>`;
  }).join('');
}

function evidenceFindingLabel(rule) {
  return ({
    'tiny-interactive-target': 'Small interactive target',
    'insufficient-text-contrast': 'Low text contrast',
    'declared-theme-contradiction': 'Theme contradiction',
    'document-horizontal-overflow': 'Horizontal page overflow',
    'nested-horizontal-scrollbars': 'Nested horizontal scrolling',
    'triple-nested-vertical-scrollbars': 'Too many vertical scroll areas',
    'clipped-x': 'Content clipped horizontally',
    'clipped-y': 'Content clipped vertically',
    'clipped-by-ancestor': 'Content cut by its container',
    'occluded': 'Content covered by another element',
    'partially-occluded': 'Content partly covered',
    'broken-image': 'Broken image',
    'broken-video': 'Broken video',
    'ttfb-above-threshold': 'Slow server response',
    'lcp-above-threshold': 'Slow main content rendering',
    'performance-metric-unavailable': 'Performance evidence unavailable',
  })[rule] || evidenceStepLabel(rule);
}

function evidenceFeedbackForImage(imageId) {
  return (state.evidenceData?.feedback || []).filter((item) => item.image_id === imageId && item.state !== 'deleted');
}

function renderEvidenceInspector(run, step, cell, screenshot) {
  const threads = evidenceFeedbackForImage(screenshot?.image_id);
  const selected = threads.find((item) => item.feedback_id === state.evidenceSelectedFeedbackId);
  const findings = cell.findings || [];
  const capture = `<section class="evidence-inspector-section"><h2>Capture details</h2><dl class="evidence-capture-facts">
    <div><dt>Viewport</dt><dd>${esc(cell.viewport?.name || '—')} · ${esc(cell.viewport?.width)} × ${esc(cell.viewport?.height)}</dd></div>
    <div><dt>Route</dt><dd>${esc(cell.final_path || cell.requested_path || '—')}</dd></div>
    <div><dt>State</dt><dd>${esc(step.label)}</dd></div>
    <div><dt>Captured</dt><dd>${esc(screenshot?.captured_at ? ago(screenshot.captured_at) : 'Unavailable')}</dd></div>
    <div><dt>Duration</dt><dd>${cell.duration_ms == null ? '—' : `${(cell.duration_ms / 1000).toFixed(1)}s`}</dd></div>
    <div><dt>Result</dt><dd>${badge(cell.outcome === 'checked' ? 'checked' : cell.outcome, cell.outcome === 'checked' ? 'ok' : 'bad')}</dd></div>
  </dl></section>`;
  const automatic = `<section class="evidence-inspector-section"><h2>Automated findings <span>${findings.length}</span></h2>${findings.length ? `<ul class="evidence-findings">${findings.map((finding) => `<li class="${esc(finding.severity)}"><button type="button" data-evidence-finding="${esc(finding.rule)}"><i></i><span><strong>${esc(evidenceFindingLabel(finding.rule))}</strong><small>${esc(finding.severity)}</small></span></button></li>`).join('')}</ul><p class="muted evidence-finding-note" id="evidence-finding-note">Choose a finding to return focus to its capture.</p>` : '<p class="muted">No automatic visual findings for this capture.</p>'}</section>`;
  const threadList = `<section class="evidence-inspector-section evidence-annotations"><h2>Annotations <span>${threads.length}</span></h2>${threads.length ? threads.map((thread, index) => `<button type="button" class="evidence-thread-summary${selected?.feedback_id === thread.feedback_id ? ' active' : ''}" data-evidence-feedback="${esc(thread.feedback_id)}"><i>${index + 1}</i><span><strong>${esc(thread.comments[0]?.body || 'Visual feedback')}</strong><small>${esc(thread.state)} · ${esc(thread.author)}</small></span></button>`).join('') : '<p class="muted">No feedback on this screenshot yet.</p>'}</section>`;
  if (!selected) {
    return `${capture}${automatic}${threadList}`;
  }
  const comments = selected.comments.map((comment) => `<article class="evidence-comment" data-comment="${esc(comment.comment_id)}"><header><strong>${esc(comment.author)}</strong><small>${esc(ago(comment.created_at))}</small></header><p>${esc(comment.body)}</p>${comment.can_edit && !comment.deleted ? `<div class="actions"><button class="btn btn-small" type="button" data-evidence-edit-comment="${esc(comment.comment_id)}">Edit</button></div>` : ''}</article>`).join('');
  const thread = `<section class="evidence-inspector-section evidence-thread"><div class="evidence-thread-head"><h2>Discussion</h2><button class="btn btn-small" type="button" data-evidence-feedback-back>All annotations</button></div><div class="evidence-thread-state">${badge(selected.state, selected.state === 'resolved' ? 'ok' : 'warn')}<a href="#/plan/${esc(state.evidenceData.repository_id)}" data-evidence-open-task="${esc(selected.task_id)}">Open Plan task →</a></div>${comments}<form id="evidence-feedback-reply"><label class="f">Reply<textarea name="body" rows="3" maxlength="2000" required></textarea></label><button class="btn" type="submit">Reply</button></form><div class="evidence-thread-actions"><button class="btn" type="button" data-evidence-state="${selected.state === 'resolved' ? 'open' : 'resolved'}">${selected.state === 'resolved' ? 'Reopen' : 'Resolve'}</button>${selected.can_delete ? '<button class="btn btn-danger" type="button" data-evidence-delete>Delete annotation</button>' : ''}</div></section>`;
  return `${capture}${automatic}${threadList}${thread}`;
}

async function loadEvidenceThumbnails(run, root = main) {
  const images = new Map();
  for (const step of state.evidenceSteps) {
    for (const cell of step.variants) {
      for (const image of Object.values(cell.screenshots || {})) {
        if (image?.status === 'available') images.set(image.image_id, image);
      }
    }
  }
  const observer = 'IntersectionObserver' in window ? new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      observer.unobserve(entry.target);
      const image = images.get(entry.target.dataset.evidenceThumb);
      evidenceImageUrl(run, image).then((url) => { if (entry.target.isConnected) entry.target.src = url; }).catch(() => {});
    }
  }, { rootMargin: '160px' }) : null;
  root.querySelectorAll('[data-evidence-thumb]').forEach((element) => {
    if (observer) observer.observe(element);
    else {
      const image = images.get(element.dataset.evidenceThumb);
      evidenceImageUrl(run, image).then((url) => { if (element.isConnected) element.src = url; }).catch(() => {});
    }
  });
}

let evidenceCanvasSession = null;
let evidenceLayoutSession = null;

function setupEvidenceLayout() {
  evidenceLayoutSession?.dispose();
  const page = $('.evidence-page', main);
  if (!page) return;
  const controller = new AbortController();
  const signal = controller.signal;
  const preferences = {};
  try { Object.assign(preferences, JSON.parse(localStorage.getItem('dc2-evidence-panels') || '{}')); } catch {}
  let fullscreen = false;
  let pendingFullscreen = false;
  let fullscreenPanels = { journey: false, details: false };
  let priorScroll;
  let blockedSiblings = [];
  let toastLocation;
  let frame;
  page.classList.add('evidence-layout');
  const expanded = (panel) => fullscreen ? fullscreenPanels[panel] : preferences[panel] ?? (panel === 'journey' || page.clientWidth >= 1050);
  const paint = () => {
    if (!page.isConnected) return;
    page.classList.toggle('evidence-layout-narrow', page.clientWidth < 1050);
    page.classList.toggle('evidence-is-fullscreen', fullscreen);
    page.style.height = `${fullscreen ? innerHeight : Math.max(520, innerHeight - Math.max(0, page.getBoundingClientRect().top) - 1)}px`;
    for (const [panel, selector, label] of [['journey', '#evidence-journey', 'journey panel'], ['details', '#evidence-inspector', 'capture details']]) {
      const visible = expanded(panel);
      $(selector, page).hidden = !visible;
      page.style.setProperty(`--evidence-${panel}-width`, visible ? (panel === 'journey' ? '230px' : '280px') : '0px');
      page.querySelectorAll(`[data-evidence-panel="${panel}"]`).forEach(button => {
        button.setAttribute('aria-expanded', String(visible));
        button.setAttribute('aria-label', `${visible ? 'Hide' : 'Show'} ${label}`);
        button.title = `${visible ? 'Hide' : 'Show'} ${label}`;
        if (!button.classList.contains('evidence-panel-close')) {
          const icon = $('.ti', button);
          icon.classList.toggle('ti-layout-sidebar-left-collapse', visible);
          icon.classList.toggle('ti-layout-sidebar-left-expand', !visible);
        }
      });
    }
    page.classList.toggle('inspector-open', expanded('details'));
    const board = $('.evidence-board', page).getBoundingClientRect();
    const imageRegion = $('#evidence-scroll', page).getBoundingClientRect();
    page.style.setProperty('--evidence-details-top', `${Math.max(0, imageRegion.top - board.top) + 4}px`);
    page.style.setProperty('--evidence-details-bottom', `${Math.max(0, board.bottom - imageRegion.bottom) + 4}px`);
    const fullButton = $('[data-evidence-fullscreen]', page);
    const fullLabel = fullscreen ? 'Exit full screen' : 'Full screen';
    fullButton.setAttribute('aria-label', fullLabel); fullButton.title = fullLabel;
    fullButton.setAttribute('aria-pressed', String(fullscreen));
    fullButton.disabled = pendingFullscreen;
    $('.evidence-fullscreen-label', fullButton).textContent = fullLabel;
    $('path', fullButton).setAttribute('d', fullscreen ? 'M4 9h5V4m6 0v5h5m0 6h-5v5m-6 0v-5H4' : 'M4 9V4h5m6 0h5v5m0 6v5h-5m-6 0H4v-5');
    const toolbar = $('.evidence-toolbar', page);
    toolbar.classList.remove('evidence-toolbar-compact');
    const needed = [...toolbar.children].reduce((width, element) => width + element.getBoundingClientRect().width + 6, 0) + 20;
    toolbar.classList.toggle('evidence-toolbar-compact', needed > toolbar.clientWidth);
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(() => {
      redrawEvidenceCanvas();
      const composer = $('#evidence-composer', page);
      if (composer && !composer.hidden) openEvidenceComposer(undefined, false);
    });
  };
  const showPanel = (panel, visible, focus = false) => {
    if (fullscreen) fullscreenPanels[panel] = visible;
    else {
      preferences[panel] = visible;
      try { localStorage.setItem('dc2-evidence-panels', JSON.stringify(preferences)); } catch {}
    }
    paint();
    if (focus) (visible && panel === 'details' ? $('.evidence-panel-close', page) : $(`[data-evidence-panel="${panel}"]:not(.evidence-panel-close)`, page))?.focus({ preventScroll: true });
  };
  const finishFullscreen = () => {
    fullscreen = false;
    for (const [element, inert] of blockedSiblings) element.inert = inert;
    blockedSiblings = [];
    if (toastLocation) {
      toastLocation.parent.insertBefore($('#toasts'), toastLocation.next?.parentNode === toastLocation.parent ? toastLocation.next : null);
      toastLocation = null;
    }
    document.body.classList.remove('evidence-fullscreen-mode');
    paint();
    if (priorScroll) window.scrollTo(priorScroll.x, priorScroll.y);
    $('[data-evidence-fullscreen]', page)?.focus({ preventScroll: true });
  };
  const toggleFullscreen = async () => {
    if (pendingFullscreen) return;
    pendingFullscreen = true;
    if (fullscreen) {
      try { if (document.fullscreenElement === page) await document.exitFullscreen(); }
      finally { finishFullscreen(); pendingFullscreen = false; paint(); }
      return;
    }
    priorScroll = { x: scrollX, y: scrollY };
    fullscreenPanels = { journey: false, details: false };
    fullscreen = true;
    const toasts = $('#toasts');
    toastLocation = { parent: toasts.parentNode, next: toasts.nextSibling };
    page.appendChild(toasts);
    for (let active = page; active.parentElement; active = active.parentElement) {
      for (const sibling of active.parentElement.children) if (sibling !== active) {
        blockedSiblings.push([sibling, sibling.inert]); sibling.inert = true;
      }
      if (active.parentElement === document.body) break;
    }
    document.body.classList.add('evidence-fullscreen-mode');
    paint();
    try { if (document.fullscreenEnabled && page.requestFullscreen) await page.requestFullscreen(); } catch {}
    finally { pendingFullscreen = false; paint(); }
    $('#evidence-canvas', page)?.focus({ preventScroll: true });
  };
  page.addEventListener('click', event => {
    const panel = event.target.closest('[data-evidence-panel]');
    if (panel) showPanel(panel.dataset.evidencePanel, !expanded(panel.dataset.evidencePanel), true);
    if (event.target.closest('[data-evidence-fullscreen]')) void toggleFullscreen();
  }, { signal });
  document.addEventListener('fullscreenchange', () => {
    if (fullscreen && document.fullscreenElement !== page) finishFullscreen();
  }, { signal });
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && fullscreen) {
      event.preventDefault(); event.stopPropagation(); void toggleFullscreen();
    } else if (event.key === 'Escape' && page.classList.contains('evidence-layout-narrow') && expanded('details') && $('#evidence-inspector', page).contains(document.activeElement)) {
      event.preventDefault(); showPanel('details', false, true);
    } else if (event.key === 'Tab' && fullscreen) {
      const controls = [...page.querySelectorAll('button:not(:disabled),a[href],input,select,textarea,[tabindex="0"]')].filter(element => element.getClientRects().length);
      const boundary = event.shiftKey ? controls[0] : controls.at(-1);
      if (document.activeElement === boundary) { event.preventDefault(); (event.shiftKey ? controls.at(-1) : controls[0])?.focus(); }
    }
  }, { signal, capture: true });
  const observer = new ResizeObserver(paint);
  observer.observe(page);
  observer.observe($('.evidence-workspace', page));
  window.addEventListener('resize', paint, { signal });
  const dispose = () => {
    if (controller.signal.aborted) return;
    controller.abort(); observer.disconnect(); cancelAnimationFrame(frame);
    for (const [element, inert] of blockedSiblings) element.inert = inert;
    if (toastLocation) toastLocation.parent.insertBefore($('#toasts'), toastLocation.next?.parentNode === toastLocation.parent ? toastLocation.next : null);
    page.classList.remove('evidence-is-fullscreen');
    document.body.classList.remove('evidence-fullscreen-mode');
    if (document.fullscreenElement === page) void document.exitFullscreen().catch(() => {});
  };
  viewAbort.signal.addEventListener('abort', dispose, { once: true });
  evidenceLayoutSession = { showDetails: () => showPanel('details', true), refresh: paint, dispose };
  paint();
}
function cloneEvidenceMarks(marks) { return JSON.parse(JSON.stringify(marks || [])); }
function clamp01(value) { return Math.max(0, Math.min(1, value)); }

function evidenceSavedMarks(imageId) {
  const threads = evidenceFeedbackForImage(imageId);
  const marks = [];
  threads.forEach((thread, threadIndex) => {
    (thread.marks || []).forEach((mark) => marks.push({
      ...mark, saved: true, feedbackId: thread.feedback_id,
      threadNumber: threadIndex + 1,
    }));
  });
  return marks;
}

function evidenceMarkBounds(mark) {
  if (mark.type === 'pin' || mark.type === 'text') return { x: mark.x, y: mark.y, width: .02, height: .02 };
  if (mark.type === 'rectangle') return mark;
  if (mark.type === 'arrow') return {
    x: Math.min(mark.x1, mark.x2), y: Math.min(mark.y1, mark.y2),
    width: Math.abs(mark.x2 - mark.x1), height: Math.abs(mark.y2 - mark.y1),
  };
  const xs = (mark.points || []).map((point) => point.x);
  const ys = (mark.points || []).map((point) => point.y);
  return { x: Math.min(...xs), y: Math.min(...ys), width: Math.max(...xs) - Math.min(...xs), height: Math.max(...ys) - Math.min(...ys) };
}

function drawEvidenceArrow(ctx, x1, y1, x2, y2, color) {
  const angle = Math.atan2(y2 - y1, x2 - x1); const head = 11;
  ctx.strokeStyle = color; ctx.lineWidth = 3; ctx.lineCap = 'round'; ctx.lineJoin = 'round';
  ctx.beginPath(); ctx.moveTo(x1, y1); ctx.lineTo(x2, y2);
  ctx.lineTo(x2 - head * Math.cos(angle - Math.PI / 6), y2 - head * Math.sin(angle - Math.PI / 6));
  ctx.moveTo(x2, y2); ctx.lineTo(x2 - head * Math.cos(angle + Math.PI / 6), y2 - head * Math.sin(angle + Math.PI / 6)); ctx.stroke();
}

function drawEvidenceMark(ctx, mark, width, height, selected = false) {
  const color = mark.color || '#f59e0b';
  ctx.save(); ctx.strokeStyle = color; ctx.fillStyle = color; ctx.lineJoin = 'round'; ctx.lineCap = 'round';
  if (mark.type === 'pin') {
    const x = mark.x * width; const y = mark.y * height;
    ctx.beginPath(); ctx.arc(x, y, 13, 0, Math.PI * 2); ctx.fill();
    ctx.fillStyle = '#07101b'; ctx.font = '700 11px system-ui'; ctx.textAlign = 'center'; ctx.textBaseline = 'middle';
    ctx.fillText(String(mark.threadNumber || '+'), x, y);
  } else if (mark.type === 'rectangle') {
    ctx.lineWidth = 3; ctx.strokeRect(mark.x * width, mark.y * height, mark.width * width, mark.height * height);
  } else if (mark.type === 'arrow') {
    drawEvidenceArrow(ctx, mark.x1 * width, mark.y1 * height, mark.x2 * width, mark.y2 * height, color);
  } else if (mark.type === 'freehand' || mark.type === 'highlight') {
    const points = mark.points || [];
    if (points.length > 1) {
      ctx.globalAlpha = mark.type === 'highlight' ? .38 : 1;
      ctx.lineWidth = mark.type === 'highlight' ? 14 : 3;
      ctx.beginPath(); ctx.moveTo(points[0].x * width, points[0].y * height);
      points.slice(1).forEach((point) => ctx.lineTo(point.x * width, point.y * height)); ctx.stroke();
    }
  } else if (mark.type === 'text') {
    const x = mark.x * width; const y = mark.y * height; const label = mark.text || '';
    ctx.font = '600 14px system-ui'; const textWidth = ctx.measureText(label).width;
    ctx.fillStyle = 'rgba(7,16,27,.9)'; ctx.fillRect(x - 4, y - 16, textWidth + 8, 22);
    ctx.fillStyle = color; ctx.fillText(label, x, y);
  }
  if (selected) {
    const box = evidenceMarkBounds(mark); const x = box.x * width; const y = box.y * height;
    const w = Math.max(18, box.width * width); const h = Math.max(18, box.height * height);
    ctx.setLineDash([5, 4]); ctx.lineWidth = 1.5; ctx.strokeStyle = '#f8fafc'; ctx.strokeRect(x - 5, y - 5, w + 10, h + 10); ctx.setLineDash([]);
    ctx.fillStyle = '#f8fafc'; ctx.fillRect(x + w - 2, y + h - 2, 8, 8);
  }
  ctx.restore();
}

function redrawEvidenceCanvas() {
  const session = evidenceCanvasSession;
  if (!session?.canvas?.isConnected || !session.image?.complete) return;
  const rect = session.image.getBoundingClientRect();
  if (!rect.width || !rect.height) return;
  const ratio = window.devicePixelRatio || 1;
  session.canvas.width = Math.round(rect.width * ratio); session.canvas.height = Math.round(rect.height * ratio);
  session.canvas.style.width = `${rect.width}px`; session.canvas.style.height = `${rect.height}px`;
  const ctx = session.canvas.getContext('2d'); ctx.setTransform(ratio, 0, 0, ratio, 0, 0); ctx.clearRect(0, 0, rect.width, rect.height);
  for (const mark of evidenceSavedMarks(session.imageId)) drawEvidenceMark(ctx, mark, rect.width, rect.height, false);
  for (const mark of state.evidenceDraftMarks) drawEvidenceMark(ctx, mark, rect.width, rect.height, mark.id === state.evidenceSelectedMarkId);
}

function evidencePoint(event, canvas) {
  const rect = canvas.getBoundingClientRect();
  return { x: clamp01((event.clientX - rect.left) / rect.width), y: clamp01((event.clientY - rect.top) / rect.height) };
}

function evidenceHitTest(point, imageId) {
  const all = [
    ...state.evidenceDraftMarks.map((mark) => ({ mark, draft: true })),
    ...evidenceSavedMarks(imageId).map((mark) => ({ mark, draft: false })),
  ];
  for (let index = all.length - 1; index >= 0; index -= 1) {
    const candidate = all[index]; const box = evidenceMarkBounds(candidate.mark);
    const pad = .018;
    if (point.x >= box.x - pad && point.x <= box.x + Math.max(box.width, .02) + pad
      && point.y >= box.y - pad && point.y <= box.y + Math.max(box.height, .02) + pad) return candidate;
  }
  return null;
}

function translateEvidenceMark(mark, dx, dy) {
  const box = evidenceMarkBounds(mark);
  dx = Math.max(-box.x, Math.min(1 - box.x - box.width, dx));
  dy = Math.max(-box.y, Math.min(1 - box.y - box.height, dy));
  if (mark.type === 'pin' || mark.type === 'text' || mark.type === 'rectangle') {
    mark.x = clamp01(mark.x + dx); mark.y = clamp01(mark.y + dy);
  } else if (mark.type === 'arrow') {
    mark.x1 = clamp01(mark.x1 + dx); mark.y1 = clamp01(mark.y1 + dy);
    mark.x2 = clamp01(mark.x2 + dx); mark.y2 = clamp01(mark.y2 + dy);
  } else {
    mark.points = mark.points.map((point) => ({ x: clamp01(point.x + dx), y: clamp01(point.y + dy) }));
  }
}

function commitEvidenceMarks(next, before = state.evidenceDraftMarks) {
  state.evidenceUndo.push(cloneEvidenceMarks(before));
  state.evidenceDraftMarks = cloneEvidenceMarks(next);
  state.evidenceRedo = [];
  updateEvidenceToolbar(); redrawEvidenceCanvas();
}

function evidenceUndo() {
  if (!state.evidenceUndo.length) return;
  state.evidenceRedo.push(cloneEvidenceMarks(state.evidenceDraftMarks));
  state.evidenceDraftMarks = state.evidenceUndo.pop(); state.evidenceSelectedMarkId = null;
  updateEvidenceToolbar(); redrawEvidenceCanvas();
}

function evidenceRedo() {
  if (!state.evidenceRedo.length) return;
  state.evidenceUndo.push(cloneEvidenceMarks(state.evidenceDraftMarks));
  state.evidenceDraftMarks = state.evidenceRedo.pop(); state.evidenceSelectedMarkId = null;
  updateEvidenceToolbar(); redrawEvidenceCanvas();
}

function updateEvidenceToolbar() {
  if (!state.evidenceDraftMarks.some((mark) => mark.id === state.evidenceSelectedMarkId)) state.evidenceSelectedMarkId = null;
  const saving = state.evidenceSavingKey === state.evidenceDraftKey && state.evidenceSavingKey !== null;
  main.querySelectorAll('[data-evidence-tool]').forEach((button) => {
    const active = button.dataset.evidenceTool === state.evidenceTool;
    button.classList.toggle('active', active); button.setAttribute('aria-pressed', String(active));
    button.disabled = saving;
  });
  const undo = $('[data-evidence-undo]', main); const redo = $('[data-evidence-redo]', main);
  const clear = $('[data-evidence-clear]', main); const zoom = $('#evidence-zoom-value', main);
  if (undo) undo.disabled = saving || !state.evidenceUndo.length;
  if (redo) redo.disabled = saving || !state.evidenceRedo.length;
  if (clear) clear.disabled = saving || !state.evidenceDraftMarks.length;
  if (zoom) zoom.textContent = `${Math.round(state.evidenceZoom * 100)}%`;
  const canvas = $('#evidence-canvas', main);
  if (canvas) {
    canvas.dataset.tool = state.evidenceTool;
    canvas.dataset.draftCount = String(state.evidenceDraftMarks.length);
    canvas.dataset.selectedMark = state.evidenceSelectedMarkId || '';
  }
  const composer = ensureEvidenceComposer();
  if (composer) composer.hidden = state.evidenceComposerDismissed || !state.evidenceDraftMarks.length && !state.evidenceDraftBody;
  const editComment = $('[data-evidence-compose]', main);
  if (editComment) editComment.disabled = saving || !state.evidenceDraftMarks.length && !state.evidenceDraftBody;
  const form = $('#evidence-feedback-create', main);
  if (form) {
    const body = state.evidenceDraftBody.trim();
    const submit = $('button[type="submit"]', form);
    if (submit) {
      submit.disabled = !!state.evidenceSavingKey || !!state.evidencePendingLabel || !state.evidenceDraftMarks.length || body.length < 3;
      submit.title = state.evidencePendingLabel ? 'Add or cancel the text label before saving.' : !state.evidenceDraftMarks.length ? 'Add a mark to attach this comment.' : '';
      const requirement = $('#evidence-feedback-requirement', form);
      requirement.textContent = submit.title; requirement.hidden = !submit.title;
    }
    form.elements.body.disabled = saving;
    $('[data-evidence-cancel]', form).disabled = saving;
  }
}

function setEvidenceZoom(value) {
  state.evidenceZoom = Math.max(.5, Math.min(4, value));
  const media = $('#evidence-media', main); if (media) media.style.width = `${state.evidenceZoom * 100}%`;
  updateEvidenceToolbar(); requestAnimationFrame(redrawEvidenceCanvas);
}

function evidenceTextEntry(point, focus = true) {
  const media = $('#evidence-media', main); if (!media) return;
  let editor = media.querySelector('.evidence-label-editor');
  if (!editor) {
    editor = document.createElement('form'); editor.className = 'evidence-label-editor';
    editor.innerHTML = '<input class="evidence-text-entry" aria-label="Annotation label" placeholder="Annotation label" maxlength="120" required><div class="actions"><button class="btn btn-small" type="button">Cancel</button><button class="btn btn-primary btn-small" type="submit">Add label</button></div>';
    const input = $('input', editor);
    input.value = state.evidencePendingLabel?.text || '';
    const cancel = () => { state.evidencePendingLabel = null; editor.remove(); updateEvidenceToolbar(); $('#evidence-canvas', main)?.focus({ preventScroll: true }); };
    $('button[type=button]', editor).addEventListener('click', cancel);
    editor.addEventListener('keydown', (event) => { if (event.key === 'Escape') { event.preventDefault(); cancel(); } });
    editor.addEventListener('submit', (event) => {
      event.preventDefault(); const text = input.value.trim();
      if (!text) { input.setCustomValidity('Enter a label.'); input.reportValidity(); return; }
      const position = { x: Number(editor.dataset.x), y: Number(editor.dataset.y) };
      const id = evidenceMarkId();
      commitEvidenceMarks([...state.evidenceDraftMarks, { id, type: 'text', color: state.evidenceColor, ...position, text }]);
      state.evidenceSelectedMarkId = id;
      state.evidencePendingLabel = null; editor.remove(); updateEvidenceToolbar(); openEvidenceComposer(position);
    });
    input.addEventListener('input', () => {
      input.setCustomValidity('');
      state.evidencePendingLabel = { point: { x: Number(editor.dataset.x), y: Number(editor.dataset.y) }, text: input.value };
    });
    media.appendChild(editor);
  }
  editor.dataset.x = String(point.x); editor.dataset.y = String(point.y);
  state.evidencePendingLabel = { point, text: $('input', editor).value };
  const visible = $('#evidence-scroll', main).getBoundingClientRect();
  const bounds = media.getBoundingClientRect();
  editor.style.left = `${Math.max(0, visible.left - bounds.left, Math.min(point.x * media.clientWidth, visible.right - bounds.left - 250))}px`;
  editor.style.top = `${Math.max(0, visible.top - bounds.top, Math.min(point.y * media.clientHeight, visible.bottom - bounds.top - 100))}px`;
  updateEvidenceToolbar();
  if (focus) $('input', editor)?.focus({ preventScroll: true });
}

function setupEvidenceCanvas(imageId) {
  evidenceCanvasSession?.observer?.disconnect();
  const priorCanvas = $('#evidence-canvas', main); const image = $('#evidence-image', main);
  if (!priorCanvas || !image) return;
  const canvas = priorCanvas.cloneNode(true); priorCanvas.replaceWith(canvas);
  const session = { canvas, image, imageId, drag: null, space: false, observer: new ResizeObserver(redrawEvidenceCanvas) };
  evidenceCanvasSession = session; session.observer.observe(image);
  const finish = (event) => {
    const drag = session.drag; if (!drag) return;
    session.drag = null;
    if (event?.type === 'pointercancel' || event?.key === 'Escape') {
      if (drag.before) state.evidenceDraftMarks = cloneEvidenceMarks(drag.before);
      updateEvidenceToolbar(); redrawEvidenceCanvas(); return;
    }
    let created = false;
    if (drag.kind === 'draw') {
      const mark = state.evidenceDraftMarks.find((item) => item.id === drag.id);
      const box = mark ? evidenceMarkBounds(mark) : null;
      if (!mark || (mark.type === 'rectangle' && (box.width < .004 || box.height < .004))
        || (mark.type === 'arrow' && box.width < .004 && box.height < .004)
        || ((mark.type === 'freehand' || mark.type === 'highlight') && mark.points.length < 2)) {
        state.evidenceDraftMarks = cloneEvidenceMarks(drag.before);
      } else {
        state.evidenceUndo.push(cloneEvidenceMarks(drag.before)); state.evidenceRedo = [];
        created = true;
      }
    } else if (drag.kind === 'move' || drag.kind === 'resize') {
      if (JSON.stringify(drag.before) !== JSON.stringify(state.evidenceDraftMarks)) {
        state.evidenceUndo.push(cloneEvidenceMarks(drag.before)); state.evidenceRedo = [];
      }
    }
    updateEvidenceToolbar(); redrawEvidenceCanvas();
    if (created) openEvidenceComposer(drag.start);
  };
  canvas.addEventListener('pointerdown', (event) => {
    if (event.button !== 0 && event.button !== 1 || state.evidenceSavingKey === state.evidenceDraftKey) return;
    const point = evidencePoint(event, canvas);
    if (session.space || event.button === 1) {
      const scroll = $('#evidence-scroll', main);
      session.drag = { kind: 'pan', clientX: event.clientX, clientY: event.clientY,
        scrollLeft: scroll.scrollLeft, scrollTop: scroll.scrollTop };
      canvas.setPointerCapture(event.pointerId); event.preventDefault(); return;
    }
    if (state.evidenceTool === 'text') { event.preventDefault(); evidenceTextEntry(point); return; }
    if (state.evidenceTool === 'select') {
      const hit = evidenceHitTest(point, imageId);
      if (!hit) { state.evidenceSelectedMarkId = null; redrawEvidenceCanvas(); return; }
      if (!hit.draft) {
        state.evidenceSelectedMarkId = null;
        state.evidenceSelectedFeedbackId = hit.mark.feedbackId; revealEvidenceDiscussion(); redrawEvidenceCanvas(); return;
      }
      state.evidenceSelectedMarkId = hit.mark.id;
      const box = evidenceMarkBounds(hit.mark);
      const resize = hit.mark.type === 'rectangle'
        && Math.abs(point.x - (box.x + box.width)) < .025
        && Math.abs(point.y - (box.y + box.height)) < .025;
      session.drag = { kind: resize ? 'resize' : 'move', id: hit.mark.id, start: point, before: cloneEvidenceMarks(state.evidenceDraftMarks) };
    } else if (state.evidenceTool === 'pin') {
      const pendingPin = state.evidenceDraftMarks.find((mark) => mark.type === 'pin');
      const pin = {
        id: pendingPin?.id || evidenceMarkId(), type: 'pin', color: state.evidenceColor,
        x: point.x, y: point.y,
      };
      commitEvidenceMarks(pendingPin ? state.evidenceDraftMarks.map((mark) => mark.id === pin.id ? pin : mark) : [...state.evidenceDraftMarks, pin]);
      state.evidenceSelectedMarkId = pin.id;
      event.preventDefault(); openEvidenceComposer(point);
    } else {
      const id = evidenceMarkId(); const before = cloneEvidenceMarks(state.evidenceDraftMarks);
      const mark = state.evidenceTool === 'rectangle'
        ? { id, type: 'rectangle', color: state.evidenceColor, x: point.x, y: point.y, width: 0, height: 0 }
        : state.evidenceTool === 'arrow'
          ? { id, type: 'arrow', color: state.evidenceColor, x1: point.x, y1: point.y, x2: point.x, y2: point.y }
          : { id, type: state.evidenceTool, color: state.evidenceColor, points: [point] };
      state.evidenceDraftMarks.push(mark); state.evidenceSelectedMarkId = id;
      session.drag = { kind: 'draw', id, start: point, before };
    }
    if (session.drag) { canvas.setPointerCapture(event.pointerId); event.preventDefault(); }
    updateEvidenceToolbar(); redrawEvidenceCanvas();
  });
  canvas.addEventListener('pointermove', (event) => {
    const drag = session.drag; if (!drag) return;
    if (drag.kind === 'pan') {
      const scroll = $('#evidence-scroll', main);
      scroll.scrollLeft = drag.scrollLeft - (event.clientX - drag.clientX);
      scroll.scrollTop = drag.scrollTop - (event.clientY - drag.clientY);
      return;
    }
    const point = evidencePoint(event, canvas); const mark = state.evidenceDraftMarks.find((item) => item.id === drag.id);
    if (!mark) return;
    if (drag.kind === 'move') {
      const original = drag.before.find((item) => item.id === drag.id); Object.assign(mark, cloneEvidenceMarks([original])[0]);
      translateEvidenceMark(mark, point.x - drag.start.x, point.y - drag.start.y);
    } else if (drag.kind === 'resize') {
      const original = drag.before.find((item) => item.id === drag.id); Object.assign(mark, cloneEvidenceMarks([original])[0]);
      mark.width = Math.max(.002, clamp01(point.x - mark.x)); mark.height = Math.max(.002, clamp01(point.y - mark.y));
    } else if (mark.type === 'rectangle') {
      mark.x = Math.min(drag.start.x, point.x); mark.y = Math.min(drag.start.y, point.y);
      mark.width = Math.abs(point.x - drag.start.x); mark.height = Math.abs(point.y - drag.start.y);
    } else if (mark.type === 'arrow') { mark.x2 = point.x; mark.y2 = point.y; }
    else if (mark.points.length < 256) mark.points.push(point);
    redrawEvidenceCanvas();
  });
  canvas.addEventListener('pointerup', finish); canvas.addEventListener('pointercancel', finish);
  canvas.addEventListener('keydown', (event) => {
    if (state.evidenceSavingKey === state.evidenceDraftKey) return;
    if (event.key === 'Escape' && session.drag) { event.preventDefault(); finish(event); return; }
    if (event.code === 'Space') { event.preventDefault(); session.space = true; canvas.dataset.panning = 'true'; return; }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'z') {
      event.preventDefault(); if (event.shiftKey) evidenceRedo(); else evidenceUndo(); return;
    }
    if (event.key === 'Delete' && state.evidenceSelectedMarkId) {
      event.preventDefault(); commitEvidenceMarks(state.evidenceDraftMarks.filter((mark) => mark.id !== state.evidenceSelectedMarkId)); state.evidenceSelectedMarkId = null; return;
    }
    if (['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key) && state.evidenceSelectedMarkId) {
      event.preventDefault(); const before = cloneEvidenceMarks(state.evidenceDraftMarks);
      const mark = state.evidenceDraftMarks.find((item) => item.id === state.evidenceSelectedMarkId);
      if (!mark) return;
      const amount = (event.shiftKey ? 10 : 1) / Math.max(canvas.clientWidth, canvas.clientHeight);
      translateEvidenceMark(mark, event.key === 'ArrowLeft' ? -amount : event.key === 'ArrowRight' ? amount : 0,
        event.key === 'ArrowUp' ? -amount : event.key === 'ArrowDown' ? amount : 0);
      state.evidenceUndo.push(before); state.evidenceRedo = []; updateEvidenceToolbar(); redrawEvidenceCanvas();
    }
  });
  canvas.addEventListener('keyup', (event) => {
    if (event.code === 'Space') { session.space = false; delete canvas.dataset.panning; }
  });
  canvas.addEventListener('blur', () => { session.space = false; delete canvas.dataset.panning; });
  redrawEvidenceCanvas();
}

function evidenceToolbar() {
  return `<div class="evidence-toolbar" role="toolbar" aria-label="Screenshot annotation tools">
    <div class="evidence-tool-group">${EVIDENCE_TOOLS.map(([tool, icon, label]) => `<button type="button" class="evidence-tool${tool === state.evidenceTool ? ' active' : ''}" data-evidence-tool="${tool}" aria-label="${label}" title="${label}" aria-pressed="${tool === state.evidenceTool}">${planIcon(icon)}<span>${label}</span></button>`).join('')}</div>
    <label class="evidence-color" title="Annotation colour">${planIcon('palette')}<span class="sr-only">Annotation colour</span><select id="evidence-color" aria-label="Annotation colour">${EVIDENCE_COLORS.map(([color, label]) => `<option value="${color}"${color === state.evidenceColor ? ' selected' : ''}>${label}</option>`).join('')}</select><i style="--mark-color:${state.evidenceColor}"></i></label>
    <div class="evidence-tool-group evidence-history"><button type="button" class="evidence-tool" data-evidence-compose aria-label="Edit comment" title="Edit comment" disabled>${planIcon('message-plus')}<span>Edit comment</span></button><button type="button" class="evidence-tool" data-evidence-undo aria-label="Undo" title="Undo" disabled>${planIcon('arrow-back-up')}</button><button type="button" class="evidence-tool" data-evidence-redo aria-label="Redo" title="Redo" disabled>${planIcon('arrow-forward-up')}</button></div>
    <div class="evidence-tool-group evidence-zoom"><button type="button" class="evidence-tool" data-evidence-zoom-out aria-label="Zoom out" title="Zoom out">${planIcon('zoom-out')}</button><output id="evidence-zoom-value">100%</output><button type="button" class="evidence-tool" data-evidence-zoom-in aria-label="Zoom in" title="Zoom in">${planIcon('zoom-in')}</button><button type="button" class="evidence-tool" data-evidence-fit aria-label="Fit screenshot" title="Fit screenshot">${planIcon('focus-centered')}</button><button type="button" class="evidence-tool" data-evidence-clear aria-label="Clear unsaved marks" title="Clear unsaved marks" disabled>${planIcon('trash')}</button></div>
    <div class="evidence-tool-group evidence-layout-controls"><button type="button" class="evidence-tool" data-evidence-panel="journey" aria-controls="evidence-journey" aria-label="Hide journey panel" title="Hide journey panel">${planIcon('layout-sidebar-left-collapse')}<span>Journey</span></button><button type="button" class="evidence-tool" data-evidence-panel="details" aria-controls="evidence-inspector" aria-label="Hide capture details" title="Hide capture details">${planIcon('layout-sidebar-left-collapse')}<span>Details</span></button><button type="button" class="evidence-tool" data-evidence-fullscreen aria-label="Full screen" title="Full screen" aria-pressed="false"><svg class="evidence-fullscreen-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 9V4h5m6 0h5v5m0 6v5h-5m-6 0H4v-5"/></svg><span class="evidence-fullscreen-label">Full screen</span></button></div>
  </div>`;
}

function renderEvidenceCurrent(step, cell) {
  const current = state.evidenceSteps.indexOf(step);
  const viewportAvailable = cell.screenshots?.viewport?.status === 'available';
  const fullAvailable = cell.screenshots?.full_page?.status === 'available';
  return `<div class="evidence-current-copy"><strong>Step ${current + 1} of ${state.evidenceSteps.length}</strong><h2>${esc(step.label)}</h2><span>${esc(cell.final_path || cell.requested_path || 'Route unavailable')}</span></div>
    <div class="evidence-current-actions"><div class="seg evidence-capture-kind" role="tablist" aria-label="Screenshot kind"><button type="button" data-evidence-kind="viewport" class="${state.evidenceScreenshotKind === 'viewport' ? 'active' : ''}"${viewportAvailable ? '' : ' disabled'}>Viewport</button><button type="button" data-evidence-kind="full_page" class="${state.evidenceScreenshotKind === 'full_page' ? 'active' : ''}"${fullAvailable ? '' : ' disabled'}>Full page</button></div><button class="btn btn-small" type="button" data-evidence-prev aria-label="Previous journey step"${current <= 0 ? ' disabled' : ''}>${planIcon('chevron-left')}</button><button class="btn btn-small" type="button" data-evidence-next aria-label="Next journey step"${current >= state.evidenceSteps.length - 1 ? ' disabled' : ''}>${planIcon('chevron-right')}</button></div>`;
}

function evidenceWorkspace(run, data) {
  const openFeedback = (data.feedback || []).filter((item) => item.state === 'open').length;
  const breadcrumb = `<a class="destination-link" href="#/tests">Tests</a><span>/</span>${workspace.active ? '' : `<strong>${esc(run.display_name)}</strong><span>/</span>`}`;
  return `<section class="evidence-page" data-ui-region="test-evidence-primary">
    <header class="evidence-page-head"><div><h1 class="evidence-breadcrumb">${breadcrumb}<strong>${esc(run.test || 'Test run')}</strong></h1><div class="evidence-run-line"><span class="mono">${esc(run.run_id)}</span>${run.isEarlierEvidence ? badge('Earlier visual run') : `${badge(run.status)}<span class="evidence-run-meta">${esc(testTierLabel(run.requested_tier))}</span><span class="evidence-run-meta">${run.readiness_eligible ? 'Release proof' : 'Diagnostic only'}</span>`}<span class="evidence-run-meta">${esc(ago(run.started_at))}</span></div></div><div class="evidence-review-state"><span>Review status</span>${openFeedback ? badge(`${openFeedback} changes requested`, 'warn') : badge('No changes requested', 'ok')}</div></header>
    <div class="evidence-board">
      <aside id="evidence-journey" class="evidence-rail" aria-label="Journey steps"><div class="evidence-rail-head"><h2>Journey</h2><span>${state.evidenceSteps.length} steps</span></div><div id="evidence-step-list">${renderEvidenceRail()}</div></aside>
      <section class="evidence-workspace" data-ui-region="test-evidence-workspace"><header id="evidence-current" class="evidence-current"></header>${evidenceToolbar()}<div id="evidence-scroll" class="evidence-scroll"><div id="evidence-media" class="evidence-media"><img id="evidence-image" alt="Selected user journey screenshot" hidden><canvas id="evidence-canvas" tabindex="0" aria-label="Screenshot annotation canvas"></canvas></div><div id="evidence-image-state" class="evidence-image-state">Loading screenshot…</div></div><section class="evidence-compare" aria-labelledby="evidence-compare-title"><div><h2 id="evidence-compare-title">Viewport comparison</h2><span>Same journey moment</span></div><div id="evidence-variants" class="evidence-variants"></div></section></section>
      <aside id="evidence-inspector" class="evidence-inspector" aria-label="Capture details and feedback"></aside>
    </div>
  </section>`;
}

function replaceEvidenceFeedback(feedback) {
  const rows = state.evidenceData.feedback || [];
  const index = rows.findIndex((item) => item.feedback_id === feedback.feedback_id);
  if (index >= 0) rows[index] = feedback; else rows.push(feedback);
}

async function evidenceMutation(button, command, args, onError = error => toast(error.message, 'bad')) {
  const runId = state.evidenceRun.run_id;
  button.disabled = true;
  try {
    const result = await api(command, {
      path: state.evidenceRun.worktree_path, run_id: state.evidenceRun.run_id, ...args,
    }, false);
    if (result.feedback && state.evidenceRun?.run_id === runId) replaceEvidenceFeedback(result.feedback);
    return result;
  } catch (error) {
    onError(error); return null;
  } finally { button.disabled = false; }
}

function bindEvidenceInspector() {
  main.querySelectorAll('[data-evidence-feedback]').forEach((button) => button.addEventListener('click', () => {
    state.evidenceSelectedFeedbackId = button.dataset.evidenceFeedback;
    evidenceLayoutSession?.showDetails();
    $('.evidence-page', main)?.classList.add('inspector-open');
    refreshEvidenceInspector(); redrawEvidenceCanvas();
  }));
  main.querySelectorAll('[data-evidence-finding]').forEach((button) => button.addEventListener('click', () => {
    main.querySelectorAll('[data-evidence-finding]').forEach((item) => item.classList.toggle('active', item === button));
    const note = $('#evidence-finding-note', main);
    if (note) note.textContent = 'This finding applies to the selected capture; private element text and selectors are not retained.';
    $('#evidence-canvas', main)?.focus();
  }));
  $('[data-evidence-feedback-back]', main)?.addEventListener('click', () => {
    state.evidenceSelectedFeedbackId = null; refreshEvidenceInspector(); redrawEvidenceCanvas();
  });
  $('#evidence-feedback-reply', main)?.addEventListener('submit', async (event) => {
    event.preventDefault(); const body = String(new FormData(event.target).get('body') || '').trim();
    if (body.length < 3) return;
    const result = await evidenceMutation(event.submitter, 'test.evidence.feedback.reply', {
      feedback_id: state.evidenceSelectedFeedbackId, body,
    });
    if (result) { refreshEvidenceInspector(); redrawEvidenceCanvas(); }
  });
  main.querySelectorAll('[data-evidence-edit-comment]').forEach((button) => button.addEventListener('click', () => {
    const article = button.closest('.evidence-comment');
    const feedback = (state.evidenceData.feedback || []).find((item) => item.feedback_id === state.evidenceSelectedFeedbackId);
    const comment = feedback?.comments.find((item) => item.comment_id === button.dataset.evidenceEditComment);
    if (!article || !comment) return;
    article.innerHTML = `<form class="evidence-comment-edit"><label class="f">Edit comment<textarea name="body" rows="4" maxlength="2000">${esc(comment.body)}</textarea></label><div class="actions"><button class="btn btn-primary btn-small" type="submit">Save</button><button class="btn btn-small" type="button" data-edit-cancel>Cancel</button></div></form>`;
    $('[data-edit-cancel]', article).addEventListener('click', refreshEvidenceInspector);
    $('form', article).addEventListener('submit', async (event) => {
      event.preventDefault(); const body = String(new FormData(event.target).get('body') || '').trim();
      if (body.length < 3) return;
      const result = await evidenceMutation(event.submitter, 'test.evidence.feedback.edit', {
        feedback_id: state.evidenceSelectedFeedbackId,
        comment_id: comment.comment_id, body,
      });
      if (result) refreshEvidenceInspector();
    });
    requestAnimationFrame(() => $('textarea', article)?.focus());
  }));
  $('[data-evidence-state]', main)?.addEventListener('click', async (event) => {
    const result = await evidenceMutation(event.currentTarget, 'test.evidence.feedback.state', {
      feedback_id: state.evidenceSelectedFeedbackId,
      state: event.currentTarget.dataset.evidenceState,
    });
    if (result) { refreshEvidenceInspector(); redrawEvidenceCanvas(); updateEvidencePageStatus(); }
  });
  $('[data-evidence-delete]', main)?.addEventListener('click', async (event) => {
    const result = await evidenceMutation(event.currentTarget, 'test.evidence.feedback.delete', {
      feedback_id: state.evidenceSelectedFeedbackId,
    });
    if (result) {
      state.evidenceSelectedFeedbackId = null; refreshEvidenceInspector();
      redrawEvidenceCanvas(); updateEvidencePageStatus(); toast('Annotation deleted', 'ok');
    }
  });
  $('[data-evidence-open-task]', main)?.addEventListener('click', (event) => {
    state.planRequestedTaskId = event.currentTarget.dataset.evidenceOpenTask;
  });
  updateEvidenceToolbar();
}

function refreshEvidenceInspector() {
  const inspector = $('#evidence-inspector', main); if (!inspector) return;
  const { step, cell, screenshot } = currentEvidenceSelection();
  inspector.innerHTML = `<button type="button" class="evidence-tool evidence-panel-close" data-evidence-panel="details" aria-label="Hide capture details" title="Hide capture details">${planIcon('x')}</button><div class="evidence-inspector-body">${renderEvidenceInspector(state.evidenceRun, step, cell, screenshot)}</div>`;
  $('h2', inspector)?.setAttribute('id', 'evidence-details-heading');
  bindEvidenceInspector();
}

function updateEvidencePageStatus() {
  const target = $('.evidence-review-state', main); if (!target) return;
  const open = (state.evidenceData.feedback || []).filter((item) => item.state === 'open').length;
  target.innerHTML = `<span>Review status</span>${open ? badge(`${open} changes requested`, 'warn') : badge('No changes requested', 'ok')}`;
}

function bindEvidenceToolbar() {
  const toolbar = $('.evidence-toolbar', main);
  if (toolbar?.dataset.evidenceBound === 'true') { updateEvidenceToolbar(); return; }
  if (toolbar) toolbar.dataset.evidenceBound = 'true';
  main.querySelectorAll('[data-evidence-tool]').forEach((button) => button.addEventListener('click', () => {
    state.evidenceTool = button.dataset.evidenceTool; state.evidenceComposerDismissed = true; updateEvidenceToolbar();
    $('#evidence-canvas', main)?.focus();
  }));
  $('#evidence-color', main)?.addEventListener('change', (event) => {
    state.evidenceColor = event.target.value;
    $('.evidence-color i', main)?.style.setProperty('--mark-color', state.evidenceColor);
    if (state.evidenceSavingKey === state.evidenceDraftKey) return;
    if (state.evidenceTool === 'select' && state.evidenceSelectedMarkId) commitEvidenceMarks(state.evidenceDraftMarks.map((mark) =>
      mark.id === state.evidenceSelectedMarkId ? { ...mark, color: state.evidenceColor } : mark));
  });
  $('[data-evidence-undo]', main)?.addEventListener('click', evidenceUndo);
  $('[data-evidence-compose]', main)?.addEventListener('click', () => openEvidenceComposer());
  $('[data-evidence-redo]', main)?.addEventListener('click', evidenceRedo);
  $('[data-evidence-clear]', main)?.addEventListener('click', () => {
    if (!state.evidenceDraftMarks.length) return;
    commitEvidenceMarks([]); state.evidenceSelectedMarkId = null;
  });
  $('[data-evidence-zoom-in]', main)?.addEventListener('click', () => setEvidenceZoom(state.evidenceZoom + .25));
  $('[data-evidence-zoom-out]', main)?.addEventListener('click', () => setEvidenceZoom(state.evidenceZoom - .25));
  $('[data-evidence-fit]', main)?.addEventListener('click', () => {
    setEvidenceZoom(1); const scroll = $('#evidence-scroll', main); if (scroll) scroll.scrollTo({ top: 0, left: 0 });
  });
  updateEvidenceToolbar();
}

async function loadMainEvidenceImage(run, screenshot) {
  const image = $('#evidence-image', main); const canvas = $('#evidence-canvas', main);
  const status = $('#evidence-image-state', main); const media = $('#evidence-media', main);
  evidenceCanvasSession?.observer?.disconnect(); evidenceCanvasSession = null;
  if (image) { image.hidden = true; image.removeAttribute('src'); }
  if (canvas) { const context = canvas.getContext('2d'); context.clearRect(0, 0, canvas.width, canvas.height); }
  if (!screenshot) { status.textContent = 'This journey step has no retained screenshot.'; status.hidden = false; return; }
  status.textContent = 'Loading screenshot…'; status.hidden = false; media.style.width = `${state.evidenceZoom * 100}%`;
  const requested = screenshot.image_id;
  try {
    const url = await evidenceImageUrl(run, screenshot);
    if (currentEvidenceSelection().screenshot?.image_id !== requested || !image?.isConnected) return;
    image.src = url; image.hidden = false; await image.decode().catch(() => {});
    status.hidden = true; setupEvidenceCanvas(requested);
    if (state.evidencePendingLabel) evidenceTextEntry(state.evidencePendingLabel.point, false);
  } catch (error) {
    status.textContent = error.message || 'Screenshot unavailable.'; status.hidden = false;
  }
}

function bindEvidenceSelection() {
  main.querySelectorAll('[data-evidence-step]').forEach((button) => button.addEventListener('click', () => {
    if (button.dataset.evidenceStep === state.evidenceStepKey) return;
    state.evidenceStepKey = button.dataset.evidenceStep; state.evidenceViewport = null;
    refreshEvidenceSelection();
  }));
  main.querySelectorAll('[data-evidence-viewport]').forEach((button) => button.addEventListener('click', () => {
    if (button.dataset.evidenceViewport === state.evidenceViewport) return;
    state.evidenceViewport = button.dataset.evidenceViewport;
    refreshEvidenceSelection();
  }));
  main.querySelectorAll('[data-evidence-kind]').forEach((button) => button.addEventListener('click', () => {
    if (button.disabled || button.dataset.evidenceKind === state.evidenceScreenshotKind) return;
    state.evidenceScreenshotKind = button.dataset.evidenceKind;
    refreshEvidenceSelection();
  }));
  $('[data-evidence-prev]', main)?.addEventListener('click', () => {
    const index = state.evidenceSteps.findIndex((item) => item.key === state.evidenceStepKey);
    if (index > 0) { state.evidenceStepKey = state.evidenceSteps[index - 1].key; state.evidenceViewport = null; refreshEvidenceSelection(); }
  });
  $('[data-evidence-next]', main)?.addEventListener('click', () => {
    const index = state.evidenceSteps.findIndex((item) => item.key === state.evidenceStepKey);
    if (index >= 0 && index < state.evidenceSteps.length - 1) { state.evidenceStepKey = state.evidenceSteps[index + 1].key; state.evidenceViewport = null; refreshEvidenceSelection(); }
  });
}

function refreshEvidenceSelection() {
  const { step, cell, screenshot } = currentEvidenceSelection(); if (!step) return;
  activateEvidenceDraft(screenshot?.image_id);
  if (screenshot?.image_id) {
    const query = new URLSearchParams(location.hash.split('?')[1] || '');
    query.set('image', screenshot.image_id);
    if (state.evidenceRun.worktree_id) query.set('worktree', state.evidenceRun.worktree_id);
    window.history.replaceState(null, '', `#/tests/${state.evidenceRunId}?${query}`);
  }
  $('#evidence-step-list', main).innerHTML = renderEvidenceRail();
  $('#evidence-current', main).innerHTML = renderEvidenceCurrent(step, cell);
  $('#evidence-variants', main).innerHTML = renderEvidenceVariants(step, cell);
  refreshEvidenceInspector(); bindEvidenceSelection(); bindEvidenceToolbar();
  for (const selector of ['#evidence-step-list', '#evidence-variants']) {
    const list = $(selector, main); const selected = $('.active', list);
    if (!selected) continue;
    const boundary = list.getBoundingClientRect(); const item = selected.getBoundingClientRect();
    if (item.left < boundary.left) list.scrollLeft -= boundary.left - item.left;
    else if (item.right > boundary.right) list.scrollLeft += item.right - boundary.right;
    if (item.top < boundary.top) list.scrollTop -= boundary.top - item.top;
    else if (item.bottom > boundary.bottom) list.scrollTop += item.bottom - boundary.bottom;
  }
  loadEvidenceThumbnails(state.evidenceRun, main); loadMainEvidenceImage(state.evidenceRun, screenshot);
}

async function viewTestEvidence(reference) {
  const [runId, queryString] = reference.split('?');
  const requestedImage = new URLSearchParams(queryString || '').get('image');
  const requestedWorktree = new URLSearchParams(queryString || '').get('worktree');
  main.innerHTML = `${pageHeading('Tests', '#/tests')}${skeleton()}`;
  let retained = workspace.retainedEvidence;
  const { runs } = retained ? { runs: [] } : await api('test.list', {});
  const owner = (runs || []).find((item) => (item.run_id === runId || item.earlier_visual_evidence?.run_id === runId) && (!requestedWorktree || item.worktree_id === requestedWorktree));
  let run = owner?.run_id === runId ? owner : owner ? {
    worktree_id: owner.worktree_id, worktree_path: owner.worktree_path,
    repository_id: owner.repository_id, display_name: owner.display_name,
    ...owner.earlier_visual_evidence, isEarlierEvidence: true,
  } : null;
  if (!run) {
    retained ||= await api('test.evidence.lookup', { run_id: runId, image_id: requestedImage || undefined, worktree_id: new URLSearchParams(queryString || '').get('worktree') || undefined });
    run = { ...retained.context, isEarlierEvidence: true };
  }
  if (state.evidenceRunId !== runId) resetEvidenceImages();
  const data = retained?.evidence || await api('test.evidence.get', { path: run.worktree_path, run_id: run.run_id });
  state.evidenceRunId = runId; state.evidenceRun = run; state.evidenceData = data;
  state.evidenceSteps = evidenceSteps(data);
  if (requestedImage) {
    let found = false;
    for (const step of state.evidenceSteps) for (const cell of step.variants) {
      const kind = ['viewport', 'full_page'].find((name) => cell.screenshots?.[name]?.image_id === requestedImage);
      if (kind) { found = true; state.evidenceStepKey = step.key; state.evidenceViewport = cell.viewport?.name; state.evidenceScreenshotKind = kind; }
    }
    if (!found) {
      main.innerHTML = `${pageHeading('Tests', '#/tests', run.display_name)}${stateBlock('empty', 'This screenshot is not available for this run.')}`;
      return;
    }
  }
  if (!state.evidenceSteps.length) {
    main.innerHTML = `${pageHeading('Tests', '#/tests', run.display_name)}${stateBlock('empty', data.issues?.length ? 'Visual evidence was invalid and could not be opened.' : 'This run did not publish visual journey evidence.')}`; return;
  }
  if (!state.evidenceSteps.some((step) => step.key === state.evidenceStepKey)) state.evidenceStepKey = state.evidenceSteps[0].key;
  main.innerHTML = evidenceWorkspace(run, data); refreshEvidenceSelection(); setupEvidenceLayout();
}

function restoreTestSettingsFocus(id) {
  const button = document.getElementById(id);
  const settings = button?.closest('details');
  if (settings) settings.open = false;
  (button?.closest('#nav') ? document.getElementById('nav-toggle') : settings?.querySelector('summary') || button)?.focus();
}

function bindTestPopovers() {
  const selector = '.test-settings[open], .test-evidence-popover[open]';
  document.addEventListener('click', (event) => {
    if (document.querySelector('dialog[open]') || event.composedPath().some((node) => node.tagName === 'DIALOG')) return;
    for (const details of main.querySelectorAll(selector)) if (!details.contains(event.target)) details.open = false;
  }, { signal: viewAbort.signal });
  document.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape' || document.querySelector('dialog[open]') || event.composedPath().some((node) => node.tagName === 'DIALOG')) return;
    for (const details of main.querySelectorAll(selector)) {
      details.open = false; details.querySelector('summary')?.focus(); event.preventDefault();
    }
  }, { signal: viewAbort.signal });
}

const viewTests = guard(async (runId = null, settings = null) => {
  if (runId) return viewTestEvidence(runId);
  main.innerHTML = `${pageHeading('Tests', '#/tests')}${skeleton()}`;
  const [{ runs }, capacity, retention] = await Promise.all([api('test.list', {}), api('test.capacity.get', {}), api('test.log.retention.get', {})]);
  await window.DevCoordinatorTests.render({
    main, runs, capacity, retention, api, esc, badge, durationMs, bytes, signal: viewAbort.signal,
    repository: workspace.active ? { name: workspace.current()?.name, ids: workspace.current()?.records.map((record) => record.repository_id) || [] } : null,
    openLogs: (run, opener) => openTestLogsDialog(run, retention, opener),
    openFiles: (run, opener, initialFile) => window.DevCoordinatorArtifacts.open(run, opener, { api, esc, bytes, signal: viewAbort.signal, highlight: highlightLogText, initialFile }),
    openCapacity: openTestCapacityDialog, openRetention: openTestLogRetentionDialog,
    bindSettings: bindTestPopovers,
  });
  if (settings === 'capacity') openTestCapacityDialog(capacity, $('#nav-toggle'));
  if (settings === 'retention') openTestLogRetentionDialog(retention, $('#nav-toggle'));
  if (settings) {
    const [pathname, queryString] = location.hash.split('?');
    const query = new URLSearchParams(queryString);
    query.delete('settings');
    window.history.replaceState(null, '', `${pathname}${query.size ? `?${query}` : ''}`);
  }
});

// --- Health --------------------------------------------------------------
const HEALTH_CONTAINER_LABELS = {
  'managed-test': 'Managed tests',
  'managed-preview': 'Managed previews',
  'managed-permanent': 'Managed permanent',
  'observed-current': 'Observed current',
  'orphaned-managed': 'Orphaned managed',
  unmanaged: 'Unmanaged',
};
const HEALTH_STORAGE_LABELS = {
  docker_shared: 'Docker shared',
  docker_images: 'Docker images',
  docker_build_cache: 'Docker build cache',
  docker_shared_volumes: 'Docker shared volumes',
  other: 'Other',
};
function healthLabel(value, labels) {
  if (labels[value]) return labels[value];
  return String(value || '').replaceAll('_', ' ').replace(/\b\w/g, (letter) => letter.toUpperCase());
}
function healthStorageBreakdown(storage) {
  const entries = Object.entries(storage || {});
  if (!entries.length) return '<span class="muted">—</span>';
  return `<dl class="health-storage-breakdown">${entries.map(([name, value]) => `<div><dt>${esc(healthLabel(name, HEALTH_STORAGE_LABELS))}</dt><dd>${bytes(value)}</dd></div>`).join('')}</dl>`;
}
function unhealthySection(summary) {
  const list = summary.unhealthy_deployments || [];
  if (!list.length) return '<section class="health-section" aria-labelledby="health-incidents-title"><h2 id="health-incidents-title">Unhealthy deployments</h2><div class="health-clear-state">All deployments are healthy.</div></section>';
  return `<section class="health-section" aria-labelledby="health-incidents-title"><h2 id="health-incidents-title">Unhealthy deployments</h2><div class="health-incident-list">${list.map((d) => `<article class="card bad-edge health-incident-card">
    <div class="cardhead"><a href="#/deployments/${esc(d.deployment_id)}"><strong>${esc(d.name)}@${esc(d.source)}</strong></a> ${d.repository_name ? `<span class="muted">in ${esc(d.repository_name)}</span>` : ''} ${badge(d.state)} ${d.observed_only ? badge('observed') : ''}</div>
    ${(d.reasons || []).length ? `<ul class="reasons">${d.reasons.map((r) => `<li><span class="mono">${esc(r.component)}</span> is ${badge(r.state, 'bad')}${r.detail ? ` — <span class="muted">${esc(r.detail)}</span>` : ''}</li>`).join('')}</ul>` : '<p class="muted">No component-level detail recorded.</p>'}
    <div class="actions">${lifecycleButtons(d.deployment_id, null, 'btn btn-small', d.state)}<a class="btn btn-small" href="#/deployments/${esc(d.deployment_id)}">details &amp; logs</a></div>
  </article>`).join('')}</div></section>`;
}
function currentAlertsSection(summary) {
  const alerts = summary.alerts || [];
  if (!alerts.length) return '<section class="health-section" aria-labelledby="health-alerts-title"><h2 id="health-alerts-title">Current alerts</h2><div class="health-clear-state">No active alerts.</div></section>';
  return `<section class="health-section" aria-labelledby="health-alerts-title"><h2 id="health-alerts-title">Current alerts</h2><ul class="health-alert-list">${alerts.map((alert) => `<li><span>${badge(alert.severity, alert.severity === 'critical' ? 'bad' : 'warn')} <span>${esc(alert.message)}</span></span><span class="muted">since ${ago(alert.opened_at)}</span></li>`).join('')}</ul></section>`;
}
function reconciliationSection(host) {
  const reconciliation = host.reconciliation || {};
  return `<section class="health-section" aria-labelledby="health-reconciliation-title"><h2 id="health-reconciliation-title">Reconciliation</h2><dl class="health-reconciliation">
    <div><dt>CPU</dt><dd><span>Managed <strong>${pct(reconciliation.managed_cpu_percent)}</strong></span><span>DevCoordinator <strong>${pct(reconciliation.daemon_cpu_percent)}</strong></span><span>Shared / unattributed <strong>${pct(reconciliation.other_cpu_percent)}</strong></span><span class="health-reconciliation-total">Host <strong>${pct(host.cpu_percent)}</strong></span></dd></div>
    <div><dt>Memory</dt><dd><span>Managed <strong>${bytes(reconciliation.managed_memory)}</strong></span><span>DevCoordinator <strong>${bytes(reconciliation.daemon_memory)}</strong></span><span>Shared / unattributed <strong>${bytes(reconciliation.other_memory)}</strong></span></dd></div>
  </dl></section>`;
}
const viewHealth = guard(async (sub) => {
  if (sub === 'containers') return viewContainers();
  main.innerHTML = `${pageHeading('Health', '#/health')}${skeleton(6)}`;
  let summary = null; let denied = null;
  try { summary = await api('health.summary', {}); } catch (e) { if (e.code !== 'permission_denied') throw e; denied = e.message; }
  const repos = await api('health.repositories', {});
  const h = summary?.host || {};
  const memFrac = h.memory_total ? h.memory_used / h.memory_total : null;
  const fsFrac = h.fs_size ? h.fs_used / h.fs_size : null;
  const containerEntries = Object.entries(summary?.container_counts || {});
  const containerTotal = containerEntries.reduce((total, [, value]) => total + Number(value || 0), 0);
  const unhealthyCount = summary?.unhealthy_deployments?.length || 0;
  const criticalCount = summary?.alerts?.filter((alert) => alert.severity === 'critical').length || 0;
  const tiles = summary ? `<div class="health-summary" data-ui-region="health-primary">
    <section class="health-panel health-capacity-panel" aria-labelledby="health-capacity-title"><h2 id="health-capacity-title">Host capacity</h2><div class="health-capacity-grid">
      <div class="health-capacity-card"><div class="k">CPU <span>${h.ncpu ?? '?'} cores</span></div><div class="health-capacity-value ${h.cpu_percent > 90 ? 'bad' : ''}"><strong>${pct(h.cpu_percent)}</strong></div>${meter((h.cpu_percent ?? 0) / 100)}</div>
      <div class="health-capacity-card"><div class="k">Memory</div><div class="health-capacity-value"><strong>${bytes(h.memory_used)}</strong><span>of ${bytes(h.memory_total)}</span></div>${meter(memFrac)}</div>
      <div class="health-capacity-card"><div class="k">Root storage</div><div class="health-capacity-value ${fsFrac > 0.9 ? 'bad' : ''}"><strong>${bytes(h.fs_used)}</strong><span>of ${bytes(h.fs_size)}</span></div>${meter(fsFrac)}</div>
      <div class="health-capacity-card"><div class="k">System load</div><div class="health-capacity-value health-load-value"><strong>${h.load_1 ?? '—'} / ${h.load_5 ?? '—'} / ${h.load_15 ?? '—'}</strong></div><div class="health-capacity-detail">1 / 5 / 15 min · Swap ${bytes(h.swap_used)}</div></div>
    </div></section>
    <section class="health-panel health-status-panel" aria-labelledby="health-status-title"><h2 id="health-status-title">Operational status</h2><div class="health-status-grid">
      <div class="health-status-item ${unhealthyCount ? 'is-critical' : ''}"><span>Unhealthy deployments</span><strong>${unhealthyCount}</strong></div>
      <div class="health-status-item ${criticalCount ? 'is-critical' : ''}"><span>Critical alerts</span><strong>${criticalCount}</strong></div>
      <div class="health-status-item"><span>Active tests</span><strong>${summary.active_tests?.length || 0}</strong></div>
      <div class="health-status-item"><span>Containers</span><strong>${containerTotal}</strong></div>
    </div>${containerEntries.length ? `<div class="health-container-mix" aria-label="Container counts by class">${containerEntries.map(([name, value]) => `<span><span>${esc(healthLabel(name, HEALTH_CONTAINER_LABELS))}</span><strong>${value}</strong></span>`).join('')}</div>` : ''}</section>
  </div>
    ${unhealthySection(summary)}
    ${currentAlertsSection(summary)}
    <section class="health-section" aria-labelledby="health-history-title"><div class="health-section-heading"><h2 id="health-history-title">History</h2>${seg(['24h', '7d', '30d'], state.healthRange, 'health-range')}</div><div id="host-history" class="chartrow">${skeleton(3)}</div></section>
    ${reconciliationSection(h)}` : stateBlock('denied', `${denied} (server-wide health is administrator-only)`);
  const rows = repos.repositories.map((r) => `<tr><td class="wrap health-repository-name" data-label="Repository"><strong>${esc(r.display_name)}</strong><div class="muted mono">${esc(r.root_path)}</div></td><td data-label="CPU"><span class="health-metric-with-trend">${pct(r.cpu_percent)} ${spark(r.trend_cpu)}</span></td><td data-label="Memory"><span class="health-metric-with-trend">${bytes(r.memory_bytes)} ${spark(r.trend_memory)}</span></td><td data-label="Storage"><span class="health-metric-with-trend">${bytes(r.storage_bytes)} ${spark(r.trend_storage)}</span></td><td data-label="Health">${badge(r.health)}</td><td class="wrap health-deployments" data-label="Deployments">${r.deployments.map((d) => `<span><a href="#/deployments/${esc(d.deployment_id)}">${esc(d.name)}@${esc(d.source)}</a> ${badge(d.state)}</span>`).join('') || '<span class="muted">none</span>'}</td></tr>`).join('');
  main.innerHTML = `<div class="health-page-heading">${pageHeading('Health', '#/health')}<a class="btn btn-small" href="#/health/containers">View containers</a></div>${tiles}<section class="health-section health-repositories" aria-labelledby="health-repositories-title"><h2 id="health-repositories-title">Repositories</h2>${rows ? `<div class="tablewrap"><table class="health-repository-table"><thead><tr><th>Repository</th><th>CPU</th><th>Memory</th><th>Storage</th><th>Health</th><th>Deployments</th></tr></thead><tbody>${rows}
    ${repos.devcoordinator ? `<tr class="health-attribution-row"><td data-label="Repository"><em>DevCoordinator</em></td><td data-label="CPU">${pct(repos.devcoordinator.cpu_percent)}</td><td data-label="Memory">${bytes(repos.devcoordinator.memory_bytes)}</td><td data-label="Storage">${bytes(repos.devcoordinator.storage_bytes)}</td><td data-label="Health"></td><td data-label="Deployments"></td></tr><tr class="health-attribution-row"><td data-label="Repository"><em>Shared / unattributed</em></td><td data-label="CPU">${pct(repos.shared_unattributed.cpu_percent)}</td><td data-label="Memory">${bytes(repos.shared_unattributed.memory_bytes)}</td><td class="wrap health-storage-cell" data-label="Storage">${healthStorageBreakdown(repos.shared_unattributed.storage)}</td><td data-label="Health"></td><td data-label="Deployments"></td></tr>` : ''}</tbody></table></div>` : stateBlock('empty', 'No repositories visible to you.')}</section>`;
  bind(main);
  bindSeg(main, 'health-range', (r) => { state.healthRange = r; render(); });
  if (summary) {
    try {
      const [cpu, mem, sto] = await Promise.all([
        metricHistory('host', 'host', 'cpu_percent', state.healthRange),
        metricHistory('host', 'host', 'memory_used', state.healthRange),
        metricHistory('host', 'host', 'storage_bytes', state.healthRange)]);
      $('#host-history').innerHTML = chart(cpu.points, pct, 'Host CPU') + chart(mem.points, bytes, 'Host memory used') + chart(sto.points, bytes, 'Storage used');
    } catch (e) { const el = $('#host-history'); if (el) el.innerHTML = stateBlock('error', e.message); }
  }
});

const viewContainers = guard(async () => {
  main.innerHTML = `${pageHeading('Health', '#/health', 'Containers')}${skeleton(6)}`;
  const { containers, counts } = await api('health.containers', {});
  const admin = state.who?.administrator;
  main.innerHTML = `${pageHeading('Health', '#/health', 'Containers')}<p><a href="#/health">← Health</a> · ${Object.entries(counts).map(([k, v]) => `${esc(k)} ${v}`).join(' · ')}</p>${containers.length ? `<div class="tablewrap"><table><thead><tr><th>Name / identity</th><th>State</th><th>Class</th><th>Repository</th><th>Deployment / test</th><th>Caller</th><th>CPU</th><th>Memory</th><th>Layer</th><th>Created</th><th>TTL</th><th>Actions</th></tr></thead><tbody>${containers.map((c) => `<tr>
    <td class="wrap"><strong>${esc(c.name)}</strong><div class="muted mono">${esc(c.id)}</div><div class="muted">${esc(c.image)}</div></td><td>${badge(c.state, c.state === 'running' ? 'ok' : '')}</td><td>${badge(c.classification, c.classification === 'unmanaged' ? 'warn' : c.classification === 'orphaned-managed' ? 'bad' : 'ok')}</td>
    <td class="mono">${esc(c.repository_id || '—')}</td><td class="mono wrap">${esc(c.deployment_id ? `${c.deployment_id}/${c.component}` : c.run_id || '—')}</td><td>${c.caller_uid ?? '—'} ${esc(c.client || '')}</td><td>${pct(c.cpu_percent)}</td><td>${bytes(c.memory_bytes)}</td><td>${bytes(c.container_layer_bytes)}</td><td class="wrap">${esc(c.created)}</td><td>${c.ttl_seconds ?? '—'}</td>
    <td class="actions">${admin && (c.classification === 'orphaned-managed' || c.classification === 'managed-test') ? `<button class="btn btn-small btn-danger" data-cmd="health.container_remove" data-args='${esc(JSON.stringify({ container_id: c.id }))}' aria-label="Remove ${esc(c.classification)} container ${esc(c.name)}">Remove ${esc(c.classification)} container</button>` : `<span class="muted">${c.classification === 'observed-current' ? 'controlled via its deployment' : 'decide manually'}</span>`}</td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', 'No containers on this host.')}`;
  bind(main);
});

// --- Codex usage ---------------------------------------------------------
const USAGE_PHASES = ['planning', 'implementation', 'testing', 'deployment', 'reporting', 'unattributed'];
const USAGE_PHASE_LABELS = {
  planning: 'Planning', implementation: 'Implementation', testing: 'Testing',
  deployment: 'Deployment', reporting: 'Reporting', unattributed: 'Unattributed',
};

function compactNumber(value) {
  if (value == null) return '—';
  const n = Number(value);
  if (Math.abs(n) >= 1e9) return `${(n / 1e9).toFixed(n >= 1e10 ? 0 : 1)}B`;
  if (Math.abs(n) >= 1e6) return `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1)}M`;
  if (Math.abs(n) >= 1e3) return `${(n / 1e3).toFixed(n >= 1e4 ? 0 : 1)}K`;
  return n.toLocaleString('en-US');
}

function durationMs(value) {
  if (value == null) return '—';
  let seconds = Math.max(0, Math.round(Number(value) / 1000));
  const hours = Math.floor(seconds / 3600); seconds %= 3600;
  const minutes = Math.floor(seconds / 60); seconds %= 60;
  if (hours) return `${hours}h ${minutes}m`;
  if (minutes) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}

function utcBucket(ms, includeDate = false) {
  const date = new Date(ms);
  const time = new Intl.DateTimeFormat('en-US', {
    hour: 'numeric', minute: '2-digit', timeZone: 'UTC',
  }).format(date);
  if (!includeDate) return time;
  const day = new Intl.DateTimeFormat('en-US', {
    month: 'short', day: 'numeric', timeZone: 'UTC',
  }).format(date);
  return `${day}, ${time}`;
}

function usageSnapshotText(coverage) {
  const snapshot = coverage?.snapshot;
  if (!snapshot) return '';
  if (!snapshot.updated_at_ms) return snapshot.refreshing ? 'Loading usage…' : 'Usage unavailable; refresh failed.';
  const saved = ago(new Date(snapshot.updated_at_ms).toISOString());
  if (snapshot.refresh_failed) return 'Refresh failed; showing saved usage · ' + saved;
  return (snapshot.refreshing ? 'Updating saved usage · ' : 'Usage snapshot · ') + saved;
}

function usageViewIdentity() {
  return [location.hash, state.codexUsageRange, state.progressPeriod].join(':');
}

function usageRefreshContext(waiting) {
  if (!waiting) return () => {};
  const opened = ['.progress-exact', '.usage-provenance'].filter((selector) => main.querySelector(selector)?.open);
  const focus = document.activeElement;
  const attribute = focus?.hasAttribute('data-progress-period') ? 'data-progress-period'
    : focus?.hasAttribute('data-codex-range') ? 'data-codex-range' : null;
  const value = attribute ? focus.getAttribute(attribute) : null;
  return () => {
    for (const selector of opened) { const details = main.querySelector(selector); if (details) details.open = true; }
    if (attribute) main.querySelector('[' + attribute + '="' + CSS.escape(value) + '"]')?.focus({ preventScroll: true });
  };
}

function continueUsageRefresh(coverages, refresh) {
  if (!coverages.some((coverage) => coverage?.snapshot?.refreshing)) return;
  const identity = usageViewIdentity();
  const controller = viewAbort;
  queueMicrotask(() => {
    if (controller !== viewAbort || controller?.signal.aborted || identity !== usageViewIdentity()) return;
    refresh();
  });
}

function coverageKind(value) {
  const coverage = value && typeof value === 'object' ? value : null;
  const stateName = coverage?.state || value;
  if (coverage?.snapshot?.refreshing) return 'indexing';
  if (coverage?.snapshot?.refresh_failed) return coverage.snapshot.updated_at_ms ? 'warn' : 'bad';
  if (coverage?.unavailable_reasons?.indexing) return 'indexing';
  if (stateName === 'unavailable' && (coverage?.unavailable_reasons?.mapping_pending || coverage?.unavailable_reasons?.mapping_unavailable)) {
    return 'setup';
  }
  return stateName === 'complete' ? 'ok' : stateName === 'partial' ? 'warn'
    : stateName === 'unavailable' ? 'bad' : '';
}

function coverageText(coverage, compact = false) {
  if (coverage.snapshot?.refreshing || coverage.snapshot?.refresh_failed) {
    if (!coverage.snapshot.updated_at_ms) return coverage.snapshot.refreshing ? 'Loading usage…' : 'Usage refresh failed';
    return coverage.snapshot.refresh_failed ? 'Saved usage · refresh failed' : 'Saved usage · updating';
  }
  const configured = Number(coverage.configured_collectors || 0);
  const included = Number(coverage.contributing_collectors || 0);
  if (coverage.unavailable_reasons?.indexing) return 'Updating usage data…';
  if (coverage.state === 'complete') {
    const total = configured || included;
    if (compact) return `All ${total} environments included`;
    return `All ${total} configured Codex ${total === 1 ? 'environment' : 'environments'} included`;
  }
  if (coverage.state === 'partial') {
    if (compact) return `${included} of ${configured} environments included`;
    return `Some usage may be missing · data from ${included} of ${configured} configured Codex environments`;
  }
  if (coverage.state === 'unobserved') {
    return compact ? 'No usage measured' : 'No usage measured in this period';
  }
  if (coverage.unavailable_reasons?.mapping_pending || coverage.unavailable_reasons?.mapping_unavailable) {
    return compact
      ? 'Not connected in all environments'
      : 'Not connected in every configured Codex environment';
  }
  if (coverage.state === 'unavailable') return 'Usage data unavailable';
  return `Some usage may be missing · data from ${included} of ${configured} configured Codex environments`;
}

function coverageExplanation(coverage) {
  const introduction = 'A Codex environment is a separately configured local Codex setup with its own usage history. This page combines environments without identifying them.';
  if (coverage.unavailable_reasons?.indexing) {
    return `${introduction} This range is still being prepared from the canonical usage histories; no missing value is counted as zero.`;
  }
  if (coverage.state === 'complete') {
    return `${introduction} Every configured environment supplied measurable data for this repository and period.`;
  }
  if (coverage.state === 'partial') {
    return `${introduction} Data from connected environments is shown. Environments that supplied no data, are not connected, or have unmeasured values are excluded, never counted as zero.`;
  }
  if (coverage.state === 'unobserved') {
    const setup = coverage.unavailable_reasons?.mapping_pending
      ? ' This repository is not connected in every configured environment.' : '';
    return `${introduction} The connected environments contained no measured usage for this repository and period.${setup}`;
  }
  if (coverage.unavailable_reasons?.mapping_pending || coverage.unavailable_reasons?.mapping_unavailable) {
    return `${introduction} This repository has not yet been connected in every configured environment.`;
  }
  if (coverage.state === 'unavailable') {
    return `${introduction} Configured environments could not supply usage data for this repository and period.`;
  }
  return `${introduction} Environments that supplied no data and unmeasured values are excluded, never counted as zero.`;
}

function bucketDataStatus(stateName) {
  if (stateName === 'complete') return 'Measured';
  if (stateName === 'partial') return 'Measured with gaps';
  if (stateName === 'unobserved') return 'Not measured';
  if (stateName === 'unavailable') return 'Unavailable';
  return 'Unknown';
}

function coverageMark(coverage, compact = false) {
  return `<span class="usage-coverage-mark ${coverageKind(coverage)}"><i aria-hidden="true"></i>${esc(coverageText(coverage, compact))}</span>${coverage.snapshot?.updated_at_ms ? `<small class="muted">Saved ${esc(ago(new Date(coverage.snapshot.updated_at_ms).toISOString()))}</small>` : ''}`;
}

function coverageHint(coverage) {
  const hintId = 'usage-coverage-hint';
  const titleId = `${hintId}-title`;
  return `<div class="usage-coverage-line"><span class="usage-coverage-status">${coverageMark(coverage)}</span><button type="button" class="usage-coverage-hint-toggle" aria-label="Explain Codex usage data completeness" aria-haspopup="dialog" aria-expanded="false" aria-controls="${hintId}" data-usage-coverage-hint-toggle>${planIcon('info-circle')}</button><div class="usage-coverage-popover" id="${hintId}" role="dialog" aria-labelledby="${titleId}" tabindex="-1" data-ui-allow-overlap="Open information hint intentionally overlays dashboard content" hidden><strong id="${titleId}" data-ui-continuation-anchor>About Codex environments</strong><p>${esc(coverageExplanation(coverage))}</p></div></div>`;
}

function bindCoverageHint(root = main) {
  const line = $('.usage-coverage-line', root);
  const toggle = $('[data-usage-coverage-hint-toggle]', root);
  const popover = $('.usage-coverage-popover', root);
  const metrics = $('.usage-metrics', root);
  if (!line || !toggle || !popover) return;
  const close = (restoreFocus = false) => {
    if (popover.hidden) return;
    popover.hidden = true;
    toggle.setAttribute('aria-expanded', 'false');
    metrics?.removeAttribute('data-ui-allow-overlap');
    document.removeEventListener('pointerdown', outside, true);
    if (restoreFocus) toggle.focus();
  };
  const outside = (event) => { if (!line.contains(event.target)) close(false); };
  const open = () => {
    popover.hidden = false;
    toggle.setAttribute('aria-expanded', 'true');
    metrics?.setAttribute('data-ui-allow-overlap', 'Open completeness hint intentionally overlays headline metrics');
    document.addEventListener('pointerdown', outside, true);
    popover.focus({ preventScroll: true });
  };
  toggle.addEventListener('click', () => popover.hidden ? open() : close(true));
  line.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape' || popover.hidden) return;
    event.preventDefault();
    close(true);
  });
  line.addEventListener('focusout', () => requestAnimationFrame(() => {
    if (!line.contains(document.activeElement)) close(false);
  }));
}

function phaseLegend() {
  return `<div class="usage-legend" aria-label="Work phase legend">${USAGE_PHASES.map((phase) => `<span><i class="usage-phase-${phase}" aria-hidden="true"></i>${USAGE_PHASE_LABELS[phase]}</span>`).join('')}</div>`;
}

function usagePhaseChart(series) {
  if (!series.some((point) => point.total_tokens > 0)) {
    return stateBlock('empty', 'No provider-reported total tokens in this window.');
  }
  const width = 1120; const height = 220;
  const left = 88; const right = 12; const top = 12; const bottom = 42;
  const iw = width - left - right; const ih = height - top - bottom;
  const max = Math.max(...series.map((point) => point.total_tokens), 1);
  const roundedMax = Math.ceil(max / Math.pow(10, Math.max(0, String(Math.floor(max)).length - 2)))
    * Math.pow(10, Math.max(0, String(Math.floor(max)).length - 2));
  const column = iw / series.length; const barWidth = Math.max(5, column * .72);
  const grid = [0, .25, .5, .75, 1].map((fraction) => {
    const y = top + ih - ih * fraction;
    return `<line x1="${left}" x2="${width - right}" y1="${y}" y2="${y}" class="usage-gridline"/><text x="${left - 8}" y="${y + 4}" text-anchor="end" class="usage-y-label">${esc(compactNumber(roundedMax * fraction))}</text>`;
  }).join('');
  const bars = series.map((point, index) => {
    const x = left + index * column + (column - barWidth) / 2;
    let y = top + ih;
    return USAGE_PHASES.map((phase) => {
      const value = Number(point.phases?.[phase] || 0);
      if (!value) return '';
      const h = (value / roundedMax) * ih; y -= h;
      return `<rect x="${x.toFixed(2)}" y="${y.toFixed(2)}" width="${barWidth.toFixed(2)}" height="${Math.max(.5, h).toFixed(2)}" class="usage-phase-${phase}"><title>${esc(`${utcBucket(point.bucket_start_ms, true)} · ${USAGE_PHASE_LABELS[phase]} · ${Number(value).toLocaleString('en-US')} tokens · ${bucketDataStatus(point.coverage)}`)}</title></rect>`;
    }).join('');
  }).join('');
  const labelEvery = Math.max(1, Math.ceil(series.length / 8));
  const labels = series.map((point, index) => {
    if (index % labelEvery && index !== series.length - 1) return '';
    const x = left + index * column + column / 2;
    return `<text x="${x.toFixed(2)}" y="${height - 13}" text-anchor="middle">${esc(utcBucket(point.bucket_start_ms, index === 0))}</text>`;
  }).join('');
  return `<div class="usage-chart-scroll" tabindex="0" aria-label="Scrollable token chart"><svg class="usage-phase-chart" viewBox="0 0 ${width} ${height}" role="img" aria-labelledby="usage-chart-title usage-chart-desc"><title id="usage-chart-title">Provider-reported total tokens by work phase</title><desc id="usage-chart-desc">Stacked UTC time buckets. Exact values are listed after the charts.</desc>${grid}${bars}${labels}<text x="4" y="${top + 3}" class="usage-axis-title">Tokens</text></svg></div>`;
}

function usageMetric(label, value, detail = '') {
  return `<div class="usage-metric"><div class="k">${esc(label)}</div><div class="v">${esc(value)}</div>${detail ? `<div class="usage-metric-detail">${detail}</div>` : ''}</div>`;
}

function activityRows(data) {
  const first = data.activities.slice(0, 9);
  for (const special of ['accounting_overhead', 'mixed', 'unknown']) {
    const row = data.activities.find((item) => item.activity === special);
    if (row && !first.includes(row)) first.push(row);
  }
  if (!first.length) return '<p class="muted">No classified token activity in this window.</p>';
  return `<div class="usage-activity-table" role="table" aria-label="Activity breakdown"><div class="usage-activity-head" role="row"><span>Work activity</span><span>Total tokens</span><span>%</span></div>${first.map((row) => {
    const label = row.activity.replaceAll('_', ' ').replace(/\b\w/g, (letter) => letter.toUpperCase());
    const percent = row.share == null ? null : row.share * 100;
    return `<div class="usage-activity-row" role="row"><span class="usage-activity-name"><i class="usage-phase-${esc(row.phase)}" aria-hidden="true"></i>${esc(label)}</span><span class="usage-activity-bar"><i class="usage-phase-${esc(row.phase)}" style="width:${Math.max(1, percent || 0).toFixed(1)}%"></i></span><strong title="${Number(row.total_tokens).toLocaleString('en-US')}">${esc(compactNumber(row.total_tokens))}</strong><span>${percent == null ? '—' : `${percent.toFixed(1)}%`}</span></div>`;
  }).join('')}</div>`;
}

function timeRails(data) {
  const rows = [
    ['Request-to-delivery wall', data.time.request_to_delivery],
    ['Execution wall union', data.time.execution_wall],
    ['Summed agent active', data.time.summed_agent_active],
  ];
  const max = Math.max(...rows.map(([, value]) => value.measured_ms), 1);
  return `<div class="usage-rails">${rows.map(([label, value], index) => `<div class="usage-rail"><div><span>${esc(label)}</span><strong>${esc(durationMs(value.measured_ms))}</strong></div><div class="usage-rail-track"><i class="usage-rail-${index}" style="width:${((value.measured_ms / max) * 100).toFixed(1)}%"></i></div>${value.unknown_intervals ? `<span class="muted">${value.unknown_intervals} unknown interval${value.unknown_intervals === 1 ? '' : 's'}</span>` : ''}</div>`).join('')}<p class="muted usage-time-note">Separate measurements; they are not added together.</p></div>`;
}

function toolOutcomeRows(data) {
  if (!data.tools.outcomes.length) return '<p class="muted">No tool outcomes in this window.</p>';
  const total = data.tools.outcomes.reduce((sum, row) => sum + row.count, 0);
  const order = ['completed', 'failed', 'interrupted', 'rejected', 'unknown'];
  const rows = [...data.tools.outcomes].sort((a, b) => order.indexOf(a.outcome) - order.indexOf(b.outcome));
  return `<div class="usage-outcomes">${rows.map((row) => `<div><span><i class="usage-outcome-${esc(row.outcome)}" aria-hidden="true"></i>${esc(row.outcome[0].toUpperCase() + row.outcome.slice(1))}</span><strong>${row.count.toLocaleString('en-US')}</strong><span>${total ? `${((row.count / total) * 100).toFixed(1)}%` : '—'}</span></div>`).join('')}<div class="usage-outcome-total"><span>Total</span><strong>${total.toLocaleString('en-US')}</strong><span>${total ? '100%' : '—'}</span></div></div>`;
}

function exactUsageTable(data) {
  return `<section class="usage-exact"><h3>Exact bucket values</h3><div class="tablewrap"><table><thead><tr><th>UTC bucket</th><th>Total</th>${USAGE_PHASES.map((phase) => `<th>${USAGE_PHASE_LABELS[phase]}</th>`).join('')}<th>Data status</th></tr></thead><tbody>${data.series.map((point) => `<tr><td>${esc(utcBucket(point.bucket_start_ms, true))}</td><td>${Number(point.total_tokens).toLocaleString('en-US')}</td>${USAGE_PHASES.map((phase) => `<td>${Number(point.phases?.[phase] || 0).toLocaleString('en-US')}</td>`).join('')}<td>${badge(bucketDataStatus(point.coverage), coverageKind(point.coverage))}</td></tr>`).join('')}</tbody></table></div></section>`;
}

const viewCodexUsageRepositories = guard(async (waitForRefresh = false) => {
  const identity = usageViewIdentity();
  if (!waitForRefresh) main.innerHTML = `${pageHeading('Codex Usage', '#/usage')}${skeleton(5)}`;
  const result = await api('usage.repositories', { range: state.codexUsageRange, ...(waitForRefresh ? { wait_for_refresh: true } : {}) });
  if (identity !== usageViewIdentity()) return;
  const restore = usageRefreshContext(waitForRefresh);
  const rows = result.repositories || [];
  const measured = (row) => ['complete', 'partial'].includes(row.coverage.state)
    || Number(row.coverage.contributing_collectors || 0) > 0;
  main.innerHTML = `<section data-ui-region="codex-usage-repositories"><div class="usage-collection-head">${pageHeading('Codex Usage', '#/usage')}${seg(['24h', '7d', '30d'], state.codexUsageRange, 'codex-range')}</div>${rows.length ? `<div class="tablewrap usage-collection-tablewrap"><table class="usage-collection-table"><thead><tr><th>Repository</th><th>Total tokens</th><th>Model requests</th><th>Tool calls</th><th>Execution time</th><th>Data included</th></tr></thead><tbody>${rows.map((row) => `<tr><td data-label="Repository"><a href="#/usage/${esc(row.repository_id)}"><strong>${esc(row.display_name)}</strong></a></td><td data-label="Total tokens">${esc(compactNumber(row.total_tokens))}</td><td data-label="Model requests">${measured(row) ? Number(row.model_requests).toLocaleString('en-US') : '—'}</td><td data-label="Tool calls">${measured(row) ? Number(row.tool_calls).toLocaleString('en-US') : '—'}</td><td data-label="Execution time">${measured(row) ? esc(durationMs(row.execution_wall_ms)) : '—'}</td><td data-label="Data included">${coverageMark(row.coverage, true)}</td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', 'No repositories are available for Codex usage analytics.')}</section>`;
  bindSeg(main, 'codex-range', (range) => {
    state.codexUsageRange = range;
    render().then(() => $(`[data-codex-range="${range}"]`, main)?.focus());
  });
  restore();
  continueUsageRefresh(rows.map((row) => row.coverage), () => viewCodexUsageRepositories(true));
});

const viewCodexUsage = guard(async (repositoryId, waitForRefresh = false) => {
  const identity = usageViewIdentity();
  if (!waitForRefresh) main.innerHTML = `<div class="usage-loading">${pageHeading('Codex Usage', '#/usage')}${skeleton(8)}</div>`;
  const [data, projectList] = await Promise.all([
    api('usage.repository', { repository_id: repositoryId, range: state.codexUsageRange, ...(waitForRefresh ? { wait_for_refresh: true } : {}) }),
    api('usage.repositories', { range: state.codexUsageRange }),
  ]);
  const projects = [...(projectList.repositories || []), {
    repository_id: repositoryId, display_name: data.display_name,
  }];
  if (identity !== usageViewIdentity()) return;
  const restore = usageRefreshContext(waitForRefresh);
  const subsets = [
    ['input', data.totals.input_tokens], ['cached', data.totals.cached_input_tokens],
    ['output', data.totals.output_tokens], ['reasoning', data.totals.reasoning_tokens],
  ].filter(([, value]) => value != null).map(([label, value]) => `${label} ${compactNumber(value)}`).join(' · ');
  main.innerHTML = `<section class="usage-dashboard" data-ui-region="codex-usage-dashboard">
    <div class="usage-context"><div class="usage-title"><span class="usage-repo-mark" aria-hidden="true">${planIcon('focus-centered')}</span><h1>${destinationLink('Codex Usage', '#/usage')}</h1><span class="usage-slash" aria-hidden="true">/</span>${projectPicker(projects, repositoryId, (id) => `#/usage/${id}`, 'usage')}</div><div class="usage-range">${seg(['24h', '7d', '30d'], state.codexUsageRange, 'codex-range')}</div><div class="usage-coverage">${coverageHint(data.coverage)}<span class="muted">${data.coverage.snapshot ? esc(usageSnapshotText(data.coverage)) : `Data current ${data.coverage.freshest_at_ms ? ago(new Date(data.coverage.freshest_at_ms).toISOString()) : '—'}`}</span></div></div>
    <div class="usage-metrics" data-ui-verify-min-content-inset="12">${usageMetric('Total tokens', compactNumber(data.totals.total_tokens))}${usageMetric('Model requests', compactNumber(data.totals.model_requests))}${usageMetric('Tool calls', compactNumber(data.totals.tool_calls))}${usageMetric('Execution time', durationMs(data.time.execution_wall.measured_ms))}</div>
    <section class="usage-primary" data-ui-region="usage-primary-trend"><div class="usage-section-title"><h2>Provider-reported total tokens by work phase</h2></div>${phaseLegend()}${usagePhaseChart(data.series)}</section>
    <div class="usage-lower"><section><h2>Activity breakdown</h2>${activityRows(data)}</section><section><h2>Time breakdown <span class="muted">(separate, not added together)</span></h2>${timeRails(data)}</section><section><h2>Tool outcomes</h2>${toolOutcomeRows(data)}</section></div>
    <details class="usage-provenance"><summary><strong>Data completeness</strong><span>${esc(coverageText(data.coverage))}</span><strong>Counting method</strong><span>Provider-reported total tokens; cached input and reasoning are subsets.</span></summary><div><p>${esc(coverageExplanation(data.coverage))}</p><p>${subsets ? esc(subsets) : 'Token subsets unavailable.'}</p><p>Schema ${esc(data.coverage.database_schemas.join(', ') || 'unavailable')} · taxonomy ${esc(data.coverage.taxonomy_versions.join(', ') || 'unavailable')}</p><p>${esc(data.semantics.time)}. Environments that supplied no data and unmeasured values are excluded rather than treated as zero.</p>${exactUsageTable(data)}</div></details>
  </section>`;
  bindProjectPicker(main);
  bindCoverageHint(main);
  if (data.coverage.snapshot && (!data.coverage.snapshot.updated_at_ms || !data.coverage.available_collectors)) {
    main.querySelectorAll('.usage-metrics, .usage-primary, .usage-lower, .usage-provenance').forEach((element) => element.remove());
    main.querySelector('.usage-coverage > .muted')?.remove();
  }
  restore();
  continueUsageRefresh([data.coverage], () => viewCodexUsage(repositoryId, true));
  bindSeg(main, 'codex-range', (range) => {
    state.codexUsageRange = range;
    render().then(() => $(`[data-codex-range="${range}"]`, main)?.focus());
  });
});

// --- Repository progress and forecasting --------------------------------
function progressDate(ms, withYear = false) {
  if (ms == null) return '—';
  return new Intl.DateTimeFormat('en-US', {
    weekday: 'short', month: 'short', day: 'numeric',
    ...(withYear ? { year: 'numeric' } : {}), timeZone: 'UTC',
  }).format(new Date(ms));
}

function progressRange(forecast) {
  if (!forecast || !['available', 'ready'].includes(forecast.state)) return 'Unavailable';
  if (forecast.earliest_at_ms === forecast.latest_at_ms) return progressDate(forecast.likely_at_ms, true);
  return `${progressDate(forecast.earliest_at_ms)} – ${progressDate(forecast.latest_at_ms, true)}`;
}

function progressPercent(value) {
  return value == null ? '—' : `${Math.round(Number(value) * 100)}%`;
}

function progressDelta(current, previous, { lowerIsBetter = false, suffix = '' } = {}) {
  if (current == null || previous == null) return '<span class="muted">comparison unavailable</span>';
  const delta = Number(current) - Number(previous);
  if (!delta) return '<span class="muted">no change</span>';
  const good = lowerIsBetter ? delta < 0 : delta > 0;
  const sign = delta > 0 ? '+' : '';
  return `<span class="progress-delta ${good ? 'good' : 'bad'}">${sign}${esc(compactNumber(delta))}${esc(suffix)}</span>`;
}

function progressForecastStrip(data) {
  const f = data.forecast;
  const available = ['available', 'ready'].includes(f.state);
  const headline = available ? `Likely release: ${progressRange(f)}` : 'Release date not available';
  const confidence = available
    ? `${f.confidence === 'low' ? 'Low' : f.confidence === 'medium' ? 'Medium' : 'High'} confidence · ${f.confidence_percent}%`
    : 'Confidence unavailable';
  const quality = [];
  quality.push(f.unestimated_tasks
    ? `${Number(f.unestimated_tasks).toLocaleString('en-US')} tasks have no estimate`
    : 'Every remaining task has an estimate');
  quality.push(f.target_date_recorded ? 'Release target date recorded' : 'No release target date is recorded');
  const note = available
    ? 'This date is provisional. It becomes more reliable as estimates and delivery evidence improve.'
    : f.explanation;
  return `<section class="progress-forecast" data-ui-region="progress-forecast" aria-label="Release forecast and forecast quality">
    <div class="progress-forecast-summary"><strong class="${available ? '' : 'muted'}">${esc(headline)}</strong><span class="progress-confidence${f.confidence === 'low' ? ' low' : ''}">${esc(confidence)}</span><span class="progress-remaining">${Number(f.remaining_tasks || 0).toLocaleString('en-US')} tasks remain</span></div>
    <div class="progress-forecast-quality"><div><strong>Forecast quality</strong><ul>${quality.map((item) => `<li>${esc(item)}</li>`).join('')}</ul></div><p>${esc(note)}</p></div>
  </section>`;
}

function progressChartValues(data) {
  let tasks = 0; let lines = 0; let tokens = 0;
  return data.series.map((point) => {
    tasks += point.tasks_completed;
    lines += point.planned_lines_completed;
    if (point.total_tokens != null) tokens += point.total_tokens;
    return {
      ...point, tasks_cumulative: tasks, lines_cumulative: lines,
      tokens_cumulative: point.total_tokens == null ? null : tokens,
    };
  });
}

function progressSegments(values, x, y) {
  const segments = []; let current = [];
  values.forEach((value, index) => {
    if (value == null) {
      if (current.length) segments.push(current);
      current = []; return;
    }
    current.push(`${x(index).toFixed(1)},${y(value).toFixed(1)}`);
  });
  if (current.length) segments.push(current);
  return segments;
}

function progressBucketLabel(ms, period) {
  const date = new Date(ms);
  if (period === 'hour') return [new Intl.DateTimeFormat('en-US', {
    hour: 'numeric', timeZone: 'UTC',
  }).format(date)];
  return [
    new Intl.DateTimeFormat('en-US', { weekday: 'short', timeZone: 'UTC' }).format(date),
    new Intl.DateTimeFormat('en-US', { month: 'short', day: 'numeric', timeZone: 'UTC' }).format(date),
  ];
}

function progressPointTarget(point, index, { x, y, width, height, period, label, values, first = 0 }) {
  const when = period === 'hour'
    ? `${progressDate(point.bucket_start_ms, true)} · ${utcBucket(point.bucket_start_ms)}–${utcBucket(point.bucket_end_ms)} UTC`
    : period === 'week' ? `${progressDate(point.bucket_start_ms)} – ${progressDate(point.bucket_end_ms - 1, true)} · UTC`
      : `${progressDate(point.bucket_start_ms, true)} · UTC`;
  return `<g class="progress-point-target" data-progress-point="${index}" data-progress-date="${esc(when)}" data-progress-label="${esc(label)}" data-progress-values="${esc(JSON.stringify(values))}" tabindex="${index === first ? 0 : -1}" role="button" aria-label="${esc(`${label}: show values for ${when}`)}"><rect x="${x.toFixed(1)}" y="${y.toFixed(1)}" width="${width.toFixed(1)}" height="${height.toFixed(1)}" rx="3"/></g>`;
}

let progressChartEvents;
function bindProgressPointValues(root) {
  progressChartEvents?.abort();
  const events = new AbortController();
  progressChartEvents = events;
  viewAbort.signal.addEventListener('abort', () => events.abort(), { once: true, signal: events.signal });
  const card = root.querySelector('.progress-pulse');
  if (!card) return;
  const tooltip = document.createElement('div');
  tooltip.id = 'progress-point-tooltip';
  tooltip.className = 'progress-point-tooltip';
  tooltip.setAttribute('role', 'tooltip');
  tooltip.dataset.uiContextualOverlay = 'Values for the hovered, focused or tapped chart bucket';
  tooltip.hidden = true;
  card.append(tooltip);
  let active = null;
  const hide = () => { tooltip.hidden = true; active?.removeAttribute('aria-describedby'); active = null; };
  const place = () => {
    if (!active) return;
    const point = active.getBoundingClientRect();
    const clip = card.querySelector('.progress-chart-scroll').getBoundingClientRect();
    if (point.right <= clip.left || point.left >= clip.right || point.bottom <= 0 || point.top >= innerHeight) { hide(); return; }
    const bounds = card.getBoundingClientRect();
    const box = tooltip.getBoundingClientRect();
    const left = Math.max(8, Math.min((point.left + point.right) / 2 - bounds.left - box.width / 2, bounds.width - box.width - 8));
    let top = point.top - bounds.top - box.height - 8;
    if (bounds.top + top < 8) top = point.bottom - bounds.top + 8;
    top = Math.max(8 - bounds.top, Math.min(top, innerHeight - bounds.top - box.height - 8));
    tooltip.style.left = `${left}px`;
    tooltip.style.top = `${top}px`;
  };
  const show = (point) => {
    active?.removeAttribute('aria-describedby');
    active = point;
    for (const sibling of point.closest('svg').querySelectorAll('[data-progress-point]')) sibling.setAttribute('tabindex', sibling === point ? '0' : '-1');
    const values = JSON.parse(point.dataset.progressValues);
    tooltip.innerHTML = `<strong>${esc(point.dataset.progressLabel)}</strong><span>${esc(point.dataset.progressDate)}</span><dl>${values.map(([label, value]) => `<div><dt>${esc(label)}</dt><dd>${esc(value)}</dd></div>`).join('')}</dl>`;
    tooltip.hidden = false;
    point.setAttribute('aria-describedby', tooltip.id);
    place();
  };
  let pointerPosition = null;
  card.addEventListener('pointermove', event => {
    const moved = !pointerPosition || event.clientX !== pointerPosition.x || event.clientY !== pointerPosition.y;
    pointerPosition = { x: event.clientX, y: event.clientY };
    const point = event.target.closest('[data-progress-point]');
    if (moved && point) show(point);
  });
  for (const point of card.querySelectorAll('[data-progress-point]')) {
    point.addEventListener('pointerenter', () => { if (!document.activeElement?.hasAttribute('data-progress-point')) show(point); });
    point.addEventListener('pointerleave', () => { if (active === point && document.activeElement !== point) hide(); });
    point.addEventListener('focus', () => show(point));
    point.addEventListener('blur', hide);
    point.addEventListener('click', () => { point.focus({ preventScroll: true }); show(point); });
    point.addEventListener('keydown', event => {
      if (event.key === 'Escape') { event.preventDefault(); hide(); return; }
      if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); show(point); return; }
      if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
      event.preventDefault();
      const points = [...point.closest('svg').querySelectorAll('[data-progress-point]')];
      const current = points.indexOf(point);
      const index = event.key === 'Home' ? 0 : event.key === 'End' ? points.length - 1 : Math.max(0, Math.min(points.length - 1, current + (event.key === 'ArrowRight' ? 1 : -1)));
      points[index].scrollIntoView({ block: 'nearest', inline: 'nearest' });
      points[index].focus({ preventScroll: true });
    });
  }
  window.addEventListener('scroll', place, { capture: true, passive: true, signal: events.signal });
  window.addEventListener('resize', place, { signal: events.signal });
  document.addEventListener('pointerdown', event => { if (!event.target.closest('[data-progress-point]')) hide(); }, { signal: events.signal });
}

function progressBarLineLane(data, {
  key, incomingKey, cumulativeKey, label, completedLabel, incomingLabel,
  detail, cls, format,
}) {
  const series = progressChartValues(data);
  const width = Math.max(760, 235 + series.length * 48);
  const height = 174; const left = 252; const right = 104;
  const top = 18; const bottom = 40; const baseline = 76;
  const chartWidth = width - left - right; const step = chartWidth / Math.max(1, series.length);
  const center = (index) => left + step * (index + .5);
  const values = series.map((point) => Number(point[key] || 0));
  const incoming = series.map((point) => Number(point[incomingKey] || 0));
  const cumulative = series.map((point) => Number(point[cumulativeKey] || 0));
  const barMax = Math.max(...values, ...incoming, 1);
  const cumulativeMax = Math.max(...cumulative, 1);
  const barY = (value) => baseline - (value / barMax) * (baseline - top - 14);
  const incomingHeight = (value) => (value / barMax) * (height - bottom - baseline - 14);
  const lineY = (value) => baseline - (value / cumulativeMax) * (baseline - top - 14);
  const barWidth = Math.max(7, Math.min(28, step * .48));
  const bars = values.map((value, index) => {
    if (!value) return '';
    const y = barY(value);
    return `<rect x="${(center(index) - barWidth / 2).toFixed(1)}" y="${y.toFixed(1)}" width="${barWidth.toFixed(1)}" height="${(baseline - y).toFixed(1)}" rx="2" class="progress-bar progress-completed-bar progress-bar-${cls}"><title>${esc(`${completedLabel} that bucket: ${format(value)} · ${utcBucket(series[index].bucket_start_ms, true)} UTC`)}</title></rect>`;
  }).join('');
  const incomingBars = incoming.map((value, index) => {
    if (!value) return '';
    return `<rect x="${(center(index) - barWidth / 2).toFixed(1)}" y="${baseline}" width="${barWidth.toFixed(1)}" height="${incomingHeight(value).toFixed(1)}" rx="2" class="progress-bar progress-incoming-bar progress-incoming-${cls}"><title>${esc(`${incomingLabel} that bucket: ${format(value)} · ${utcBucket(series[index].bucket_start_ms, true)} UTC`)}</title></rect>`;
  }).join('');
  const barLabels = values.map((value, index) => {
    if (!value) return '';
    const y = barY(value);
    return `<text x="${center(index).toFixed(1)}" y="${Math.max(top + 9, y - 5).toFixed(1)}" text-anchor="middle" class="progress-bar-value">${esc(format(value))}</text>`;
  }).join('');
  const incomingLabels = incoming.map((value, index) => {
    if (!value) return '';
    const y = Math.min(height - bottom - 2, baseline + incomingHeight(value) + 11);
    return `<text x="${center(index).toFixed(1)}" y="${y.toFixed(1)}" text-anchor="middle" class="progress-bar-value progress-incoming-value">${esc(format(value))}</text>`;
  }).join('');
  const linePoints = cumulative.map((value, index) => `${center(index).toFixed(1)},${lineY(value).toFixed(1)}`);
  const dots = cumulative.map((value, index) => `<circle cx="${center(index).toFixed(1)}" cy="${lineY(value).toFixed(1)}" r="3" class="progress-running-dot progress-running-${cls}"><title>${esc(`${completedLabel} running total: ${format(value)} · ${utcBucket(series[index].bucket_start_ms, true)} UTC`)}</title></circle>`).join('');
  const every = Math.max(1, Math.ceil(series.length / 8));
  const labels = series.map((point, index) => {
    if (index % every && index !== series.length - 1) return '';
    const centerX = center(index).toFixed(1);
    const parts = progressBucketLabel(point.bucket_start_ms, data.period);
    return `<text x="${centerX}" y="${height - (parts.length > 1 ? 24 : 14)}" text-anchor="middle">${parts.map((part, partIndex) => `<tspan x="${centerX}" dy="${partIndex ? 12 : 0}">${esc(part)}</tspan>`).join('')}</text>`;
  }).join('');
  const total = cumulative.at(-1) || 0;
  const incomingTotal = incoming.reduce((sum, value) => sum + value, 0);
  const completedVerb = completedLabel.split(' ').at(-1).toLowerCase();
  const incomingVerb = incomingLabel.split(' ').at(-1).toLowerCase();
  const empty = total || incomingTotal ? '' : `<text x="${left + chartWidth / 2}" y="${baseline - 8}" text-anchor="middle" class="progress-chart-empty">No recorded movement in this period</text>`;
  const pointTargets = series.map((point, index) => progressPointTarget(point, index, {
    x: center(index) - step / 2, y: 8, width: step, height: height - bottom - 8, period: data.period, label,
    values: [[completedLabel, values[index].toLocaleString('en-US')], [incomingLabel, incoming[index].toLocaleString('en-US')], ['Completed running total', cumulative[index].toLocaleString('en-US')]],
  })).join('');
  return `<svg class="progress-pulse-chart progress-bar-line-chart" style="min-width:${width}px" viewBox="0 0 ${width} ${height}" role="group" aria-label="${esc(label)} by ${esc(data.period)}: completed above the baseline, ${esc(incomingLabel.toLowerCase())} below, with completed running total"><title>${esc(label)} by ${esc(data.period)}</title><desc>Solid bars above the baseline show completed work. Outlined bars below show incoming work. The thin line shows the completed running total. Exact values follow the chart.</desc><line x1="${left}" x2="${width - right}" y1="${baseline}" y2="${baseline}" class="progress-grid-h progress-zero-line"/><text x="14" y="${top + 14}" class="progress-lane-title" data-ui-verify-svg-overlap="lane title stays outside plotted work">${esc(label)}</text><text x="14" y="${top + 34}" class="progress-lane-detail" data-ui-verify-svg-overlap="lane detail stays outside plotted work">${esc(detail)}</text><text x="${width - 10}" y="${top + 22}" text-anchor="end" class="progress-lane-value progress-running-${cls}">${esc(format(total))}</text><text x="${width - 10}" y="${top + 38}" text-anchor="end" class="progress-lane-detail">${esc(completedVerb)}</text><text x="${width - 10}" y="${baseline + 24}" text-anchor="end" class="progress-lane-value progress-incoming-${cls}">${esc(format(incomingTotal))}</text><text x="${width - 10}" y="${baseline + 40}" text-anchor="end" class="progress-lane-detail">${esc(incomingVerb)}</text><polyline points="${linePoints.join(' ')}" class="progress-running-line progress-running-${cls}"/>${dots}${bars}${incomingBars}${barLabels}${incomingLabels}${empty}${labels}${pointTargets}</svg>`;
}

function progressEvidenceLane(data, {
  key, label, detail, cls, format, fixedMax = null, summarize = null,
}) {
  const values = data.series.map((point) => point[key]);
  const observedValues = values.filter((value) => value != null);
  const observed = observedValues.length > 0;
  const coverage = cls === 'tests' ? data.coverage.tests : data.coverage.tokens;
  const snapshotText = cls === 'tokens' ? usageSnapshotText(coverage) : '';
  const unavailable = (coverage.snapshot?.refreshing || coverage.snapshot?.refresh_failed ? snapshotText : '') || (cls === 'tests'
    ? 'No test runs recorded for this period.'
    : coverage.state === 'unobserved' ? 'No token data recorded for this period.' : 'Some token data is missing.');
  if (!observed) return `<div class="progress-evidence-lane" data-progress-evidence="${esc(cls)}"><div><strong>${esc(label)}</strong><small>${esc(detail)}</small></div><p>${esc(unavailable)}</p><strong>—</strong></div>`;
  const width = Math.max(650, 220 + values.length * 38); const height = 58;
  const left = 168; const right = 92; const top = 8; const bottom = 8;
  const chartWidth = width - left - right;
  const x = (index) => left + (values.length === 1 ? chartWidth / 2 : index * chartWidth / (values.length - 1));
  const max = fixedMax || Math.max(...values.filter((value) => value != null), 1);
  const y = (value) => top + (height - top - bottom) - (Number(value) / max) * (height - top - bottom - 10);
  const polylines = progressSegments(values, x, y).map((points) => `<polyline points="${points.join(' ')}" class="progress-evidence-line progress-running-${cls}"/>`).join('');
  const dots = values.map((value, index) => value == null ? '' : `<circle cx="${x(index).toFixed(1)}" cy="${y(value).toFixed(1)}" r="3" class="progress-evidence-dot progress-running-${cls}"><title>${esc(`${label}: ${format(value)} · ${utcBucket(data.series[index].bucket_start_ms, true)} UTC`)}</title></circle>`).join('');
  const pointTargets = values.map((value, index) => value == null ? '' : progressPointTarget(data.series[index], index, {
    x: x(index) - 16, y: Math.max(0, Math.min(height - 28, y(value) - 14)), width: 32, height: 28,
    period: data.period, label, first: values.findIndex(item => item != null),
    values: cls === 'tests'
      ? [[label, new Intl.NumberFormat('en-US', { style: 'percent', maximumFractionDigits: 2 }).format(value)], ['Tests passed', Number(data.series[index].tests_passed).toLocaleString('en-US')], ['Recorded test runs', Number(data.series[index].test_runs).toLocaleString('en-US')]]
      : [['Tokens', Number(value).toLocaleString('en-US')], ['Token data', bucketDataStatus(data.series[index].token_coverage)]],
  })).join('');
  const summary = summarize
    ? summarize(observedValues)
    : [...values].reverse().find((value) => value != null);
  const note = snapshotText || (coverage.state === 'complete' ? detail : cls === 'tokens'
    ? 'Measured total; missing buckets stay blank.'
    : 'Some data is missing; gaps stay blank.');
  return `<div class="progress-evidence-lane" data-progress-evidence="${esc(cls)}"><div><strong>${esc(label)}</strong><small>${esc(note)}</small></div><svg class="progress-evidence-chart" viewBox="0 0 ${width} ${height}" role="group" aria-label="${esc(label)} across this period"><line x1="${left}" x2="${width - right}" y1="${height - bottom}" y2="${height - bottom}" class="progress-grid-h"/>${polylines}${dots}${pointTargets}</svg><strong aria-label="${esc(`${label} measured in this period: ${format(summary)}`)}">${esc(format(summary))}</strong></div>`;
}

function progressPulseChart(data) {
  if (!data.series.length) return stateBlock('empty', 'No progress buckets in this period.');
  return `<div class="progress-chart-legend"><span><i class="progress-legend-bar" aria-hidden="true"></i>Green = tasks</span><span><i class="progress-legend-bar progress-legend-lines" aria-hidden="true"></i>Blue = planned lines</span><span><i class="progress-legend-bar progress-legend-incoming" aria-hidden="true"></i>Solid above = completed · outlined below = incoming</span><span><i class="progress-legend-line" aria-hidden="true"></i>Line = completed running total</span></div><div class="progress-chart-scroll" tabindex="0" aria-label="Scrollable daily progress charts"><div class="progress-chart-canvas">${progressBarLineLane(data, { key: 'tasks_completed', incomingKey: 'tasks_created', cumulativeKey: 'tasks_cumulative', label: 'Tasks finished and created', completedLabel: 'Tasks finished', incomingLabel: 'Tasks created', detail: 'Finished above · created below', cls: 'tasks', format: compactNumber })}${progressBarLineLane(data, { key: 'planned_lines_completed', incomingKey: 'planned_lines_added', cumulativeKey: 'lines_cumulative', label: 'Planned lines completed and added', completedLabel: 'Planned lines completed', incomingLabel: 'Planned lines added', detail: 'Completed above · added below', cls: 'lines', format: compactNumber })}${progressEvidenceLane(data, { key: 'test_pass_rate', label: 'Test pass rate', detail: 'Recorded terminal runs', cls: 'tests', format: progressPercent, fixedMax: 1 })}${progressEvidenceLane(data, { key: 'total_tokens', label: 'Token use', detail: 'Provider tokens · selected period', cls: 'tokens', format: compactNumber, summarize: (items) => items.reduce((sum, value) => sum + Number(value), 0) })}<p class="progress-missing-note">Missing information stays blank and is never counted as zero.</p></div></div>`;
}

function progressReleaseWork(data, repositoryId) {
  const rows = data.release_work.slice(0, 5);
  const title = data.forecast.release ? 'Work in this release' : 'Open plan work';
  if (!rows.length) return `<section class="progress-release-work" data-ui-region="progress-release-work"><div class="progress-section-heading"><div><h2>${title}</h2><span>Shown in Plan order</span></div></div>${stateBlock('empty', data.forecast.release ? 'No open work remains in this release.' : 'No open work is recorded.')}</section>`;
  const body = rows.map((task) => {
    const selected = task.task_id === state.progressSelectedTaskId;
    const estimate = task.estimated_loc == null
      ? 'Estimate missing' : `~${Number(task.estimated_loc).toLocaleString('en-US')} lines`;
    return `<button type="button" class="progress-work-row${selected ? ' selected' : ''}" data-progress-task="${esc(task.task_id)}" aria-pressed="${selected}"><span class="progress-work-title"><strong>${esc(task.title)}</strong><small>${planBadge(task.status)} <span>${esc(estimate)}</span>${task.reopened ? ` ${badge('reopened', 'warn')}` : ''}${task.elaboration_needed ? ` ${badge('elaboration needed', 'warn')}` : ''}</small></span><span class="ti ti-chevron-right" aria-hidden="true"></span></button>`;
  }).join('');
  return `<section class="progress-release-work" data-ui-region="progress-release-work"><div class="progress-section-heading"><div><h2>${title}</h2><span>Shown in Plan order</span></div></div><div class="progress-work-column-label">Task</div>${body}<footer>Showing ${rows.length} of ${Number(data.release_work.length).toLocaleString('en-US')} open tasks <a href="#/plan/${esc(repositoryId)}">Open full plan →</a></footer></section>`;
}

function progressComparison(data) {
  const current = data.comparison.current; const previous = data.comparison.previous;
  const items = [
    ['Tasks completed', compactNumber(current.tasks_completed), compactNumber(previous.tasks_completed), progressDelta(current.tasks_completed, previous.tasks_completed)],
    ['Tasks created', compactNumber(current.tasks_created), compactNumber(previous.tasks_created), progressDelta(current.tasks_created, previous.tasks_created, { lowerIsBetter: true })],
    ['Planned lines completed', compactNumber(current.planned_lines_completed), compactNumber(previous.planned_lines_completed), progressDelta(current.planned_lines_completed, previous.planned_lines_completed)],
    ['Planned lines added', compactNumber(current.planned_lines_added), compactNumber(previous.planned_lines_added), progressDelta(current.planned_lines_added, previous.planned_lines_added, { lowerIsBetter: true })],
  ];
  const previousRange = `${progressDate(data.window.comparison_start_ms)} – ${progressDate(data.window.start_ms - 1, true)}`;
  return `<section class="progress-comparison" aria-labelledby="progress-comparison-title"><h2 id="progress-comparison-title">Compared with the previous matching period (${esc(previousRange)})</h2><div>${items.map(([label, now, before, delta]) => `<article><span>${esc(label)}</span><strong>${esc(now)}</strong><small>Previous ${esc(before)}</small>${delta}</article>`).join('')}</div></section>`;
}

function progressExactTable(data) {
  return `<details class="progress-exact"><summary>Exact values and counting method</summary><div class="tablewrap"><table><thead><tr><th>UTC bucket</th><th>Tasks done</th><th>Tasks created</th><th>Planned lines done</th><th>Planned lines added</th><th>Tests</th><th>Pass rate</th><th>Total tokens</th><th>Token data</th></tr></thead><tbody>${data.series.map((point) => `<tr><td>${esc(utcBucket(point.bucket_start_ms, true))}</td><td>${point.tasks_completed}</td><td>${point.tasks_created}</td><td>${Number(point.planned_lines_completed).toLocaleString('en-US')}</td><td>${Number(point.planned_lines_added).toLocaleString('en-US')}</td><td>${point.test_runs}</td><td>${point.test_pass_rate == null ? '—' : progressPercent(point.test_pass_rate)}</td><td>${point.total_tokens == null ? '—' : Number(point.total_tokens).toLocaleString('en-US')}</td><td>${esc(bucketDataStatus(point.token_coverage))}</td></tr>`).join('')}</tbody></table></div><p>${esc(data.semantics.lines)}. Planned lines added means ${esc(data.semantics.lines_added)}. ${esc(data.semantics.tests)}. ${esc(data.semantics.tokens)}. ${esc(data.semantics.forecast)}.</p></details>`;
}

function renderProgressDashboard(data, projects, repositoryId) {
  const visibleWork = data.release_work.slice(0, 5);
  if (!visibleWork.some((task) => task.task_id === state.progressSelectedTaskId)) {
    state.progressSelectedTaskId = visibleWork[0]?.task_id || null;
  }
  const selected = data.release_work.find((task) => task.task_id === state.progressSelectedTaskId) || null;
  const periodLabels = { hour: 'Hour', day: 'Day', week: 'Week' };
  const context = `<header class="progress-context" data-ui-region="progress-context"><div class="progress-identity"><h1>${destinationLink('Progress', '#/progress')}</h1><span aria-hidden="true">/</span>${projectPicker(projects, repositoryId, (id) => `#/progress/${id}`, 'progress')}</div><output class="progress-window">${esc(progressDate(data.window.start_ms))} – ${esc(progressDate(data.window.end_ms, true))} · UTC</output><div class="progress-period">${seg(['hour', 'day', 'week'], state.progressPeriod, 'progress-period', (period) => periodLabels[period])}</div><div class="progress-actions"><button class="btn btn-primary" type="button" data-progress-open-task${selected ? '' : ' disabled'}>Open selected in plan</button><a href="#/plan/${esc(repositoryId)}">Open full plan →</a></div></header>`;
  const workspace = `<div class="progress-workspace" data-ui-region="progress-primary"><section class="progress-pulse"><div class="progress-section-heading"><div><h2>Daily progress</h2><span>Aligned ${esc(state.progressPeriod)}ly evidence · UTC</span></div></div>${progressPulseChart(data)}</section>${progressReleaseWork(data, repositoryId)}</div>`;
  main.innerHTML = `<section class="progress-dashboard">${context}${progressForecastStrip(data)}${workspace}${progressComparison(data)}${progressExactTable(data)}</section>`;
  bindProjectPicker(main);
  bindProgressPointValues(main);
  bindSeg(main, 'progress-period', (period) => { state.progressPeriod = period; viewProgress(repositoryId).then(() => $(`[data-progress-period="${period}"]`, main)?.focus()); });
  main.querySelectorAll('[data-progress-task]').forEach((button) => button.addEventListener('click', () => {
    state.progressSelectedTaskId = button.dataset.progressTask;
    main.querySelectorAll('[data-progress-task]').forEach((row) => {
      const active = row.dataset.progressTask === state.progressSelectedTaskId;
      row.classList.toggle('selected', active);
      row.setAttribute('aria-pressed', String(active));
    });
    const open = $('[data-progress-open-task]', main);
    if (open) open.disabled = !state.progressSelectedTaskId;
  }));
  $('[data-progress-open-task]', main)?.addEventListener('click', () => {
    if (!state.progressSelectedTaskId) return;
    state.planRequestedTaskId = state.progressSelectedTaskId;
    location.hash = `#/plan/${repositoryId}`;
  });
}

const viewProgressRepositories = guard(async () => {
  main.innerHTML = `${pageHeading('Progress', '#/progress')}${skeleton(5)}`;
  const { repositories } = await api('progress.repositories', {});
  main.innerHTML = `<section data-ui-region="progress-repositories">${pageHeading('Progress', '#/progress')}${repositories.length ? `<div class="tablewrap"><table><thead><tr><th>Repository</th><th>Next release</th><th>Plan progress</th><th>Open tasks</th><th></th></tr></thead><tbody>${repositories.map((row) => `<tr><td><a href="#/progress/${esc(row.repository_id)}"><strong>${esc(row.display_name)}</strong></a></td><td>${row.next_release ? `${esc(row.next_release.name)} ${planBadge(row.next_release.status)}` : '<span class="muted">No release planned</span>'}</td><td>${row.planned_lines_total ? `${meter(row.planned_lines_done / row.planned_lines_total)}<span class="muted">${locN(row.planned_lines_done)} of ${locN(row.planned_lines_total)} planned lines done</span>` : '<span class="muted">Nothing sized yet</span>'}</td><td>${row.open_tasks}</td><td><a class="btn btn-small" href="#/progress/${esc(row.repository_id)}">View progress</a></td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', 'No repositories are available for progress analytics.')}</section>`;
});

const viewProgress = guard(async (repositoryId, waitForRefresh = false) => {
  const identity = usageViewIdentity();
  if (state.progressRepositoryId !== repositoryId) {
    state.progressRepositoryId = repositoryId;
    state.progressSelectedTaskId = null;
  }
  if (!waitForRefresh) main.innerHTML = `<div class="progress-loading">${pageHeading('Progress', '#/progress')}${skeleton(8)}</div>`;
  const [data, list] = await Promise.all([
    api('progress.repository', { repository_id: repositoryId, period: state.progressPeriod, ...(waitForRefresh ? { wait_for_refresh: true } : {}) }),
    api('progress.repositories', {}),
  ]);
  const projects = [...(list.repositories || []), { repository_id: repositoryId, display_name: data.display_name }];
  if (identity !== usageViewIdentity()) return;
  const restore = usageRefreshContext(waitForRefresh);
  renderProgressDashboard(data, projects, repositoryId);
  restore();
  continueUsageRefresh([data.coverage.tokens], () => viewProgress(repositoryId, true));
});

// --- Bugs ----------------------------------------------------------------
const viewBugs = guard(async () => {
  main.innerHTML = `${pageHeading('Bugs', '#/bugs')}${skeleton()}`;
  const { bugs } = await api('bug.list', {});
  main.innerHTML = `${pageHeading('Bugs', '#/bugs')}<h2>Open bugs</h2>${bugs.length ? `<div class="tablewrap"><table><thead><tr><th>Component</th><th>Summary</th><th>Expected / actual</th><th>Steps</th><th>Seen</th><th>Correlations</th><th></th></tr></thead><tbody>${bugs.map((b) => `<tr><td>${esc(b.component)}</td><td class="wrap"><strong>${esc(b.summary)}</strong><div class="muted mono">${esc(b.bug_id)} · ${esc(b.reporter)}</div></td><td class="wrap">${esc(b.expected)}<br><span class="muted">${esc(b.actual)}</span></td><td class="wrap">${esc(b.steps)}</td><td>${b.occurrences}× · ${ago(b.last_seen_at)}</td><td class="mono wrap">${esc(Object.entries(b.correlations || {}).map(([k, v]) => `${k}=${v}`).join(' ') || '—')}</td><td><button class="btn btn-small" data-cmd="bug.close" data-args='${esc(JSON.stringify({ bug_id: b.bug_id }))}'>close</button></td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', 'No open bugs.')}
    <h2>Report a bug</h2><form class="inline" id="bug-form">${['component', 'summary', 'expected', 'actual', 'steps'].map((f) => `<label class="f">${f}<${f === 'steps' ? 'textarea' : 'input'} name="${f}" required ${f === 'steps' ? '></textarea>' : '>'}</label>`).join('')}<button class="btn" type="submit">Report</button></form><p class="muted">Bounded atomic records only: no secrets, raw logs, or private paths.</p>`;
  bind(main);
  $('#bug-form').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const btn = ev.target.querySelector('button');
    const args = Object.fromEntries(new FormData(ev.target).entries());
    await act(btn, 'bug.report', args, () => render());
  });
});

// --- Administration ------------------------------------------------------
const viewAdmin = guard(async () => {
  main.innerHTML = `${pageHeading('Administration', '#/admin')}${skeleton(6)}`;
  const [users, deployments, telegram] = await Promise.all([api('user.list', {}), api('deployment.list', {}), api('telegram.list', {})]);
  const depOptions = deployments.deployments.map((d) => `<option value="${esc(d.deployment_id)}">${esc(d.name)}@${esc(d.source)}</option>`).join('');
  const roleOptions = users.roles.map((r) => `<option>${esc(r)}</option>`).join('');
  main.innerHTML = `${pageHeading('Administration', '#/admin')}
    <h2>Users</h2>${users.users.length ? `<div class="tablewrap"><table><thead><tr><th>E-mail</th><th>Administrator</th><th>Grants</th><th>Last seen</th><th></th></tr></thead><tbody>${users.users.map((u) => `<tr><td class="wrap mono">${esc(u.email)}</td><td>${u.administrator ? badge('administrator', 'ok') : ''}</td><td class="wrap">${u.grants.map((g) => `<span class="badge">${esc(g.role)}</span> <span class="mono">${esc(g.deployment_id)}</span> <button class="btn btn-small" data-cmd="grant.remove" data-args='${esc(JSON.stringify({ email: u.email, deployment_id: g.deployment_id }))}' aria-label="Remove ${esc(g.role)} grant for ${esc(u.email)}">Remove grant</button>`).join('<br>') || '<span class="muted">none</span>'}</td><td>${ago(u.last_seen_at)}</td><td><button class="btn btn-small btn-danger" data-cmd="user.remove" data-args='${esc(JSON.stringify({ email: u.email }))}' aria-label="Remove user ${esc(u.email)}">Remove user</button></td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', 'No users.')}
    <form class="inline" id="grant-form"><label class="f">e-mail<input name="email" required></label><label class="f">deployment<select name="deployment_id" required>${depOptions}</select></label><label class="f">role<select name="role">${roleOptions}</select></label><button class="btn" type="submit">Set grant</button></form>
    <h2>Invitations</h2>${users.invitations.length ? `<div class="tablewrap"><table><thead><tr><th>E-mail</th><th>Administrator</th><th>Grants</th><th>Expires</th><th></th></tr></thead><tbody>${users.invitations.map((i) => `<tr><td class="mono wrap">${esc(i.email)}</td><td>${i.administrator ? 'yes' : ''}</td><td class="wrap mono">${esc(i.grants.map((g) => `${g.role}:${g.deployment_id}`).join(' ') || '—')}</td><td>${esc(i.expires_at)}</td><td><button class="btn btn-small" data-cmd="user.remove" data-args='${esc(JSON.stringify({ email: i.email }))}'>revoke</button></td></tr>`).join('')}</tbody></table></div>` : '<p class="muted">No outstanding invitations.</p>'}
    <form class="inline" id="invite-form"><label class="f">e-mail<input name="email" type="email" required></label><label class="f">deployment<select name="deployment_id"><option value="">(none)</option>${depOptions}</select></label><label class="f">role<select name="role">${roleOptions}</select></label><label class="f">administrator<input type="checkbox" name="administrator"></label><button class="btn" type="submit">Invite</button></form>
    <h2>Telegram</h2><p class="muted">Bot ${telegram.configured ? 'configured' : 'not configured'} · outbox pending ${telegram.outbox_pending} · last poll ${ago(telegram.last_poll_at)} ${telegram.last_error ? `· <span class="badge bad">${esc(telegram.last_error)}</span>` : ''}</p>
    ${telegram.chats.length ? `<div class="tablewrap"><table><thead><tr><th>Chat</th><th>E-mail</th><th>Subscriptions</th><th></th></tr></thead><tbody>${telegram.chats.map((c) => `<tr><td>${c.chat_id} <span class="muted">${esc(c.label || '')}</span></td><td class="mono wrap">${esc(c.email)}</td><td class="wrap">${c.subscriptions.map((s) => `<span class="badge">${esc(s)}</span> <button class="btn btn-small" data-cmd="telegram.unsubscribe" data-args='${esc(JSON.stringify({ chat_id: c.chat_id, scope: s }))}'>×</button>`).join(' ') || '<span class="muted">none</span>'}</td><td></td></tr>`).join('')}</tbody></table></div>` : '<p class="muted">No linked chats. Send /start to the bot to get a link code.</p>'}
    <form class="inline" id="link-form"><label class="f">link code<input name="code" required></label><label class="f">e-mail<input name="email" type="email" required></label><button class="btn" type="submit">Link chat</button></form>
    <form class="inline" id="sub-form"><label class="f">chat id<input name="chat_id" type="number" required></label><label class="f">scope<input name="scope" placeholder="server | deployment:&lt;id&gt; | repository:&lt;id&gt;" required></label><button class="btn" type="submit">Subscribe</button></form>
    <h2>Server</h2><div id="server" class="muted">${skeleton(1)}</div>`;
  bind(main);
  const submit = (form, cmd, build) => $(form).addEventListener('submit', async (ev) => { ev.preventDefault(); const fd = new FormData(ev.target); await act(ev.target.querySelector('button'), cmd, build(fd), () => render()); });
  submit('#grant-form', 'grant.set', (fd) => ({ email: fd.get('email'), deployment_id: fd.get('deployment_id'), role: fd.get('role') }));
  submit('#invite-form', 'user.invite', (fd) => ({ email: fd.get('email'), administrator: fd.get('administrator') === 'on', grants: fd.get('deployment_id') ? [{ deployment_id: fd.get('deployment_id'), role: fd.get('role') }] : [] }));
  submit('#link-form', 'telegram.link', (fd) => ({ code: fd.get('code'), email: fd.get('email') }));
  submit('#sub-form', 'telegram.subscribe', (fd) => ({ chat_id: Number(fd.get('chat_id')), scope: fd.get('scope') }));
  try {
    const [ping, edge] = await Promise.all([api('ping', {}), fetch('/healthz').then((r) => r.json())]);
    $('#server').innerHTML = `daemon ${esc(ping.daemon_version)} · schema ${ping.schema_version} · route document generation ${edge.route_generation} (${esc(edge.source)})`;
  } catch (e) { $('#server').textContent = e.message; }
});

// --- Plan (completion ledger, releases, previews) ------------------------
function locN(n) { return Number(n).toLocaleString('en-US'); }
function loc(n) { return n == null ? '' : `~${locN(n)} lines`; }
const PLAN_WORDS = { planned: 'planned', in_progress: 'being built', done: 'done', dropped: 'dropped', requested: 'preview requested', delivered: 'delivered' };
const PLAN_BADGE = { done: 'ok', delivered: 'ok', in_progress: 'warn', requested: 'warn' };
function planBadge(status) { return badge(PLAN_WORDS[status] || status, PLAN_BADGE[status] ?? ''); }
function planElaborationMark(task) {
  return `<span class="plan-elaboration-mark" data-elaboration-mark="${esc(task.task_id)}" data-ui-continuation-anchor tabindex="-1">${task.elaboration_needed ? badge('elaboration needed', 'warn') : ''}</span>`;
}
function planElaborationButton(task, context = '') {
  const requested = !!task.elaboration_needed;
  const label = requested ? 'Requested' : 'Elaborate';
  const explanation = requested
    ? `A clearer explanation has been requested for ${task.title}`
    : `Ask the agent to explain ${task.title} in simpler language`;
  return `<button class="btn btn-small plan-elaborate ${esc(context)}" type="button" data-elaborate-task="${esc(task.task_id)}" data-ui-continuation-anchor aria-label="${esc(explanation)}" title="${esc(explanation)}"${requested ? ' aria-disabled="true"' : ''}>${planIcon('message-plus')}<span class="plan-elaborate-label">${label}</span></button>`;
}

const viewPlanPicker = guard(async (kind) => {
  const title = kind === 'decisions' ? 'Decisions' : 'Plan';
  const href = kind === 'decisions' ? '#/decisions' : '#/plan';
  main.innerHTML = `${pageHeading(title, href)}${skeleton()}`;
  const { repositories } = await api('plan.overview', {});
  if (!repositories.length) { main.innerHTML = `${pageHeading(title, href)}${stateBlock('empty', 'No repositories visible to you.')}`; return; }
  main.innerHTML = `${pageHeading(title, href)}<p class="muted">Pick a repository.</p><div class="tablewrap"><table><thead><tr><th>Repository</th><th>Now building</th><th>Progress</th><th>Open tasks</th><th></th></tr></thead><tbody>${repositories.map((r) => `<tr>
    <td class="wrap"><a href="#/${kind}/${esc(r.repository_id)}"><strong>${esc(r.display_name)}</strong></a></td>
    <td class="wrap">${r.current_release ? `${esc(r.current_release.name)} ${planBadge(r.current_release.status)}` : '<span class="muted">no releases planned yet</span>'}${r.preview_requested ? ` ${badge('preview requested', 'warn')}` : ''}${r.elaboration_request_count ? ` ${badge(`${r.elaboration_request_count} need explanation`, 'warn')}` : ''}</td>
    <td>${r.loc_total ? `${meter(r.loc_done / r.loc_total)}<span class="muted">done ${locN(r.loc_done)} of ${locN(r.loc_total)} lines</span>` : '<span class="muted">nothing sized yet</span>'}</td>
    <td>${r.open_tasks}</td>
    <td class="actions"><a class="btn btn-small" href="#/plan/${esc(r.repository_id)}">plan</a><a class="btn btn-small" href="#/decisions/${esc(r.repository_id)}">decisions</a></td></tr>`).join('')}</tbody></table></div>`;
});

// Layout is pure: leaf tasks advance a global lines-of-code cursor, parents
// span their descendants, releases group in sequence, backlog trails.
function computeGantt(releases, tasks, collapsed) {
  const known = new Set(releases.map((r) => r.release_id));
  const byId = new Map(tasks.map((t) => [t.task_id, t]));
  const groupOf = (t) => (t.release_id && known.has(t.release_id) ? t.release_id : null);
  const inTree = (t) => { const p = byId.get(t.parent_task_id); return !!p && groupOf(p) === groupOf(t); };
  const order = (a, b) => (a.position - b.position) || (a.seq - b.seq);
  const kids = new Map();
  for (const t of tasks) {
    if (!inTree(t)) continue;
    if (!kids.has(t.parent_task_id)) kids.set(t.parent_task_id, []);
    kids.get(t.parent_task_id).push(t);
  }
  kids.forEach((list) => list.sort(order));
  const rows = []; const groups = []; let cursor = 0; let unsizedCount = 0;
  const walk = (t, depth, hidden) => {
    const children = kids.get(t.task_id) || [];
    const row = { task: t, depth, isParent: children.length > 0, hidden, start: cursor, width: 0, subtreeLoc: 0, doneLoc: 0, unsizedCount: 0, isUnsized: false, collapsed: collapsed.has(t.task_id) };
    rows.push(row);
    if (!children.length) {
      if (t.estimated_loc == null) {
        row.isUnsized = true;
        row.unsizedCount = 1;
        unsizedCount += 1;
      } else {
        row.width = t.estimated_loc;
        row.subtreeLoc = row.width;
        row.doneLoc = t.status === 'done' ? row.width : 0;
        cursor += row.width;
      }
    } else {
      for (const c of children) {
        const child = walk(c, depth + 1, hidden || row.collapsed);
        row.subtreeLoc += child.subtreeLoc;
        row.doneLoc += child.doneLoc;
        row.unsizedCount += child.unsizedCount;
      }
      row.width = cursor - row.start;
    }
    return row;
  };
  for (const release of [...releases, null]) {
    const gid = release ? release.release_id : null;
    const group = { release, start: cursor };
    const mark = rows.length;
    for (const t of tasks.filter((x) => groupOf(x) === gid && !inTree(x)).sort(order)) walk(t, 0, false);
    group.rows = rows.slice(mark);
    group.end = cursor;
    group.unsizedCount = group.rows.filter((row) => !row.isParent && row.isUnsized).length;
    if (release || group.rows.length) groups.push(group);
  }
  return { groups, total: cursor, unsizedCount };
}

function planIcon(name) {
  return `<span class="ti ti-${esc(name)}" aria-hidden="true"></span>`;
}

function planCanvasWidth(total) {
  return Math.round(Math.max(960, total * 0.16) * state.planZoom);
}

function planRowProgress(row) {
  const total = row.subtreeLoc || row.width || row.task.estimated_loc || 0;
  const done = row.isParent ? row.doneLoc : (row.task.status === 'done' ? total : 0);
  return { total, done, percent: total ? Math.round((done / total) * 100) : 0 };
}

function planSelectionTray(selectedRow, releaseById, admin) {
  if (!selectedRow) return '';
  const task = selectedRow.task;
  const release = releaseById.get(task.release_id);
  const releaseOpen = !release || release.status !== 'delivered';
  const movable = admin && task.status !== 'done' && releaseOpen;
  const editable = admin && (task.status === 'planned' || task.status === 'in_progress') && releaseOpen;
  const canEstimate = editable && !selectedRow.isParent;
  const progress = planRowProgress(selectedRow);
  const estimateButton = canEstimate
    ? `<button class="btn btn-small" type="button" data-resize-task="${esc(task.task_id)}">${planIcon('arrow-right')}${task.estimated_loc == null ? 'Add estimate' : 'Resize'}</button>` : '';
  const actionButtons = admin ? `${planElaborationButton(task, 'plan-elaborate-tray')}${movable ? `<button class="btn btn-small" type="button" data-move-task="${esc(task.task_id)}">${planIcon('arrows-move')}Move</button>` : ''}${estimateButton}${editable ? `<button class="btn btn-small btn-danger" data-cmd="task.update" data-args='${esc(JSON.stringify({ task_id: task.task_id, status: 'dropped' }))}' aria-label="Drop task ${esc(task.title)}">${planIcon('trash')}Drop task</button>` : ''}` : '';
  const actions = actionButtons ? `<div class="plan-selection-actions"><span class="plan-selection-label">Actions</span><div class="actions">${actionButtons}</div></div>` : '';
  const estimateText = selectedRow.unsizedCount
    ? (selectedRow.isParent ? `${selectedRow.subtreeLoc ? `${loc(selectedRow.subtreeLoc)} + ` : ''}${selectedRow.unsizedCount} ${selectedRow.unsizedCount === 1 ? 'job' : 'jobs'} not estimated` : 'Not estimated yet')
    : esc(loc(progress.total));
  const progressBlock = selectedRow.isUnsized
    ? `<div class="plan-selection-detail"><span class="plan-selection-label">Size</span><strong>Not estimated yet</strong><span class="muted">Shown in the non-proportional chart band.</span></div>`
    : `<div class="plan-selection-detail"><span class="plan-selection-label">Progress</span><strong>${locN(progress.done)} / ${locN(progress.total)} done (${progress.percent}%)</strong><progress max="${Math.max(progress.total, 1)}" value="${progress.done}"></progress></div>`;
  return `<section class="plan-selection${state.planSelectionCollapsed ? ' collapsed' : ''}" data-ui-region="selected-task" aria-label="Selected task details">
    <div class="plan-selection-heading"><span class="plan-selection-grip">${planIcon('grip-vertical')}</span><strong data-ui-continuation-anchor>${esc(task.title)}</strong><span class="plan-selection-status">${planBadge(task.status)}${planElaborationMark(task)}</span><span class="muted">${estimateText}</span></div>
    ${progressBlock}
    <div class="plan-selection-detail plan-selection-impact"><span class="plan-selection-label">Why it matters</span><span>${task.impact ? esc(task.impact) : '<span class="muted">No explanation recorded.</span>'}</span></div>
    <div class="plan-selection-detail"><span class="plan-selection-label">Release</span><span>${release ? esc(release.name) : 'Not scheduled yet'}</span>${release ? planBadge(release.status) : ''}</div>
    ${actions}
    <button class="plan-selection-toggle" type="button" data-plan-selection-toggle aria-expanded="${!state.planSelectionCollapsed}" aria-label="${state.planSelectionCollapsed ? 'Expand' : 'Collapse'} selected task details">${planIcon(state.planSelectionCollapsed ? 'chevron-up' : 'chevron-down')}</button>
  </section>`;
}

const viewPlan = guard(async (repoId) => {
  const requestedTaskId = state.planRequestedTaskId;
  state.planRequestedTaskId = null;
  if (state.planRepositoryId !== repoId) {
    state.planRepositoryId = repoId;
    state.planSelectedTaskId = requestedTaskId;
    state.planScrollLeft = 0;
    state.planScrollTop = 0;
    state.planSelectionCollapsed = window.matchMedia('(max-width: 900px)').matches;
  } else if (requestedTaskId) {
    state.planSelectedTaskId = requestedTaskId;
    state.planSelectionCollapsed = false;
  }
  main.innerHTML = `<div class="plan-loading">${pageHeading('Plan', '#/plan')}${skeleton(6)}</div>`;
  const [model, projectList] = await Promise.all([
    api('plan.overview', { repository_id: repoId }),
    workspace.active ? {} : api('plan.overview', {}),
  ]);
  const projects = [...(projectList.repositories || []), {
    repository_id: repoId, display_name: model.display_name,
  }];
  const admin = !!state.who?.administrator;
  const { groups, total, unsizedCount } = computeGantt(model.releases, model.tasks, state.collapsed);
  const rows = groups.flatMap((g) => g.rows);
  const releaseById = new Map(model.releases.map((r) => [r.release_id, r]));
  const unsizedShare = unsizedCount ? (total ? 0.18 : 1) : 0;
  const sizedShare = 1 - unsizedShare;
  const pctOf = (v) => `${(total ? (v / total) * sizedShare * 100 : 0).toFixed(3)}%`;
  const unsizedLeft = `${(sizedShare * 100 + (unsizedShare ? 1 : 0)).toFixed(3)}%`;
  const unsizedWidth = `${Math.max(0, unsizedShare * 100 - (unsizedShare ? 2 : 0)).toFixed(3)}%`;
  let selectedRow = rows.find((row) => row.task.task_id === state.planSelectedTaskId);
  if (!selectedRow) {
    selectedRow = rows.find((row) => row.isParent && row.task.status !== 'done')
      || rows.find((row) => row.task.status === 'in_progress')
      || rows.find((row) => row.task.status !== 'done')
      || rows[0]
      || null;
    state.planSelectedTaskId = selectedRow?.task.task_id || null;
  }
  const doneLoc = rows.filter((row) => !row.isParent).reduce((sum, row) => sum + row.doneLoc, 0);
  const requested = model.preview_requested || [];
  const requestControl = requested.length
    ? `<span class="plan-requested">${planBadge('requested')} <span class="muted">${ago(requested[0].requested_at)}</span></span>`
    : (admin ? `<button class="btn btn-primary" data-cmd="release.request" data-args='${esc(JSON.stringify({ repository_id: repoId }))}'>Request preview</button>` : '');
  const requestNotice = requested.length
    ? `<div class="plan-preview-notice" role="status">Preview requested. The agent will put the current work online; a link appears on the preview release when it is ready.</div>`
    : '';
  const elaborationCount = (model.elaboration_requests || []).length;
  const elaborationNotice = `<div class="plan-elaboration-notice" data-plan-elaboration-notice role="status"${elaborationCount ? '' : ' hidden'}>${elaborationCount ? `${elaborationCount} ${elaborationCount === 1 ? 'task needs' : 'tasks need'} a clearer explanation. Agents see these requests whenever they read or update the plan.` : ''}</div>`;

  const releaseHead = (group) => {
    const release = group.release;
    const droppable = admin && (!release || release.status !== 'delivered');
    const locationStyle = `style="--release-end:${pctOf(group.end)}"`;
    const where = release?.url && /^https:\/\//.test(release.url)
      ? `<a class="plan-release-location" ${locationStyle} href="${esc(release.url)}" target="_blank" rel="noopener">Open the app ↗</a>`
      : (release?.status === 'delivered' && release.port ? `<span class="plan-release-location" ${locationStyle}>runs on server port ${Number(release.port)}</span>` : '');
    const measuredProgress = release
      ? (release.loc_total ? `${locN(release.loc_done)} / ${locN(release.loc_total)} lines done` : `${release.tasks_done} / ${release.tasks_total} tasks done`)
      : `${locN(group.end - group.start)} lines`;
    const progress = `${measuredProgress}${group.unsizedCount ? ` · ${group.unsizedCount} not estimated` : ''}`;
    const releaseBar = total && group.end > group.start
      ? `<div class="grelbar ${release?.status === 'delivered' ? 'delivered' : ''}" style="left:${pctOf(group.start)};width:${pctOf(group.end - group.start)}"><span>${release ? esc(release.name) : 'Not scheduled yet'}</span></div>`
      : '';
    return `<div class="grow grel"${droppable ? ` data-drop-release="${release ? esc(release.release_id) : ''}"` : ''}>
      <div class="glabel">
        <div class="plan-release-copy"><strong title="${release ? esc(release.name) : 'Not scheduled yet'}">${release ? esc(release.name) : 'Not scheduled yet'}</strong><span class="plan-release-meta">${release ? planBadge(release.status) : ''}${release?.kind === 'preview' && release.status !== 'requested' ? ` ${badge('preview')}` : ''}<span class="muted" title="${esc(progress)}">${esc(progress)}</span></span></div>
      </div>
      <div class="gtrack">${releaseBar}${where}</div>
    </div>`;
  };

  const taskRow = (row) => {
    const task = row.task;
    const release = releaseById.get(task.release_id);
    const releaseOpen = !release || release.status !== 'delivered';
    const movable = admin && task.status !== 'done' && releaseOpen;
    const editable = admin && (task.status === 'planned' || task.status === 'in_progress') && releaseOpen;
    const pointerResizable = editable && !row.isParent && task.estimated_loc != null;
    const selected = task.task_id === state.planSelectedTaskId;
    const progress = planRowProgress(row);
    const collapse = row.isParent
      ? `<button class="plan-tree-toggle" type="button" data-collapse="${esc(task.task_id)}" data-ui-continuation-anchor aria-expanded="${!row.collapsed}" aria-label="${row.collapsed ? 'Show' : 'Hide'} subtasks of ${esc(task.title)}" title="${row.collapsed ? 'Show subtasks' : 'Hide subtasks'}">${planIcon(row.collapsed ? 'chevron-right' : 'chevron-down')}</button>`
      : '<span class="plan-tree-toggle-spacer" aria-hidden="true"></span>';
    const drag = movable
      ? `<span class="plan-drag-handle" draggable="true" data-drag-task="${esc(task.task_id)}" title="Drag to move task" aria-label="Drag ${esc(task.title)} to move it">${planIcon('grip-vertical')}</span>`
      : '<span class="plan-drag-spacer" aria-hidden="true"></span>';
    const metaLoc = row.isParent
      ? `${row.subtreeLoc ? loc(row.subtreeLoc) : ''}${row.subtreeLoc && row.unsizedCount ? ' + ' : ''}${row.unsizedCount ? `${row.unsizedCount} not estimated` : ''}`
      : (loc(task.estimated_loc) || 'not estimated');
    const common = `role="button" tabindex="0" data-select-task="${esc(task.task_id)}" aria-pressed="${selected}" aria-label="Select ${esc(task.title)}"`;
    const hover = `data-hover-task="${esc(task.task_id)}" data-hover-title="${esc(task.title)}" data-hover-status="${esc(PLAN_WORDS[task.status] || task.status)}" data-hover-loc="${esc(metaLoc)}" data-hover-progress="${esc(row.isUnsized ? 'Progress is not calculated until this work is estimated.' : `${locN(progress.done)} / ${locN(progress.total)} done (${progress.percent}%)`)}" data-hover-elaboration="${task.elaboration_needed ? 'true' : 'false'}"`;
    const sizedBar = row.width ? (row.isParent
      ? `<div class="gbar parent${selected ? ' selected' : ''}" style="left:${pctOf(row.start)};width:${pctOf(row.width)}" ${common}><span class="gdone" style="width:${progress.percent}%"></span></div>`
      : `<div class="gbar ${esc(task.status)}${selected ? ' selected' : ''}" style="left:${pctOf(row.start)};width:${pctOf(row.width)}" ${common} ${hover}>
          <span class="gdone" style="width:${progress.percent}%"></span><span class="gbar-label">${esc(task.title)}</span>
          ${pointerResizable ? `<button type="button" class="gresize" data-resize-handle="${esc(task.task_id)}" aria-label="Drag to resize ${esc(task.title)}" title="Drag to resize estimate"></button>` : ''}
        </div>`) : '';
    const unsizedBar = row.unsizedCount ? `<div class="gbar unsized ${row.isParent ? 'parent ' : ''}${esc(task.status)}${selected ? ' selected' : ''}" style="left:${unsizedLeft};width:${unsizedWidth}" ${common} ${hover}><span class="gbar-label">${row.isParent ? `${row.unsizedCount} not estimated` : esc(task.title)}</span></div>` : '';
    const bar = `${sizedBar}${unsizedBar}`;
    return `<div class="grow gtask${row.hidden ? ' ghidden' : ''}${selected ? ' selected' : ''}" data-task-row="${esc(task.task_id)}">
      <div class="glabel" style="--task-depth:${row.depth}">${drag}${collapse}<button type="button" class="plan-task-select" data-select-task="${esc(task.task_id)}" aria-pressed="${selected}">
        <span class="plan-task-title">${esc(task.title)}</span><span class="plan-task-meta" title="${esc(`${metaLoc} · ${PLAN_WORDS[task.status] || task.status}`)}"><span title="${esc(metaLoc)}">${esc(metaLoc)}</span>${planBadge(task.status)}${task.kind === 'user_feedback' ? ` ${badge('your request')}` : ''}</span>
      </button>${admin ? planElaborationButton(task, 'plan-elaborate-row') : (task.elaboration_needed ? planElaborationMark(task) : '')}</div>
      <div class="gtrack">${bar}</div>
    </div>`;
  };

  const tickCount = 8;
  const ticks = Array.from({ length: tickCount }, (_, index) => {
    const value = total ? Math.round((total * index) / (tickCount - 1)) : 0;
    return { left: `${(index / (tickCount - 1)) * sizedShare * 100}%`, value };
  });
  const gridLines = total ? ticks.slice(1, -1).map((tick) => `<i style="left:${tick.left}"></i>`).join('') : '';
  const axisTicks = total ? ticks.map((tick) => `<span style="left:${tick.left}">${locN(tick.value)}</span>`).join('') : '';
  const unsizedAxis = unsizedCount ? `<span class="plan-axis-unsized" style="left:${unsizedLeft};width:${unsizedWidth}">Not estimated</span>` : '';
  const navigatorIcon = state.planNavigatorCollapsed ? 'layout-sidebar-left-expand' : 'layout-sidebar-left-collapse';
  const toolbar = `<div class="plan-toolbar" role="toolbar" aria-label="Timeline controls">
    <button class="plan-tool${state.planMode === 'select' ? ' active' : ''}" type="button" data-plan-mode="select" aria-pressed="${state.planMode === 'select'}" title="Select tasks">${planIcon('pointer')}<span class="sr-only">Select tasks</span></button>
    <button class="plan-tool${state.planMode === 'pan' ? ' active' : ''}" type="button" data-plan-mode="pan" aria-pressed="${state.planMode === 'pan'}" title="Pan timeline">${planIcon('hand-stop')}<span class="sr-only">Pan timeline</span></button>
    <span class="plan-tool-group" aria-label="Zoom controls"><button class="plan-tool" type="button" data-plan-zoom="out" title="Zoom out">${planIcon('minus')}<span class="sr-only">Zoom out</span></button><output id="plan-zoom-value" aria-live="polite">${state.planFit ? 'Fit' : `${Math.round(state.planZoom * 100)}%`}</output><button class="plan-tool" type="button" data-plan-zoom="in" title="Zoom in">${planIcon('plus')}<span class="sr-only">Zoom in</span></button></span>
    <button class="plan-tool" type="button" data-plan-zoom="fit" title="Fit the whole timeline">${planIcon('focus-centered')}<span class="sr-only">Fit timeline</span></button>
    <button class="plan-tool plan-nav-tool" type="button" data-plan-nav-toggle data-ui-continuation-anchor aria-pressed="${state.planNavigatorCollapsed}" title="${state.planNavigatorCollapsed ? 'Show' : 'Hide'} task navigator">${planIcon(navigatorIcon)}<span class="sr-only">${state.planNavigatorCollapsed ? 'Show' : 'Hide'} task navigator</span></button>
  </div>`;
  const chartWidth = planCanvasWidth(total);
  const gantt = `<div class="plan-workspace${state.planNavigatorCollapsed ? ' navigator-collapsed' : ''}" style="--glabel:${state.planNavigatorCollapsed ? 0 : state.planNavigatorWidth}px;--chart-width:${chartWidth}px" data-ui-region="plan-primary">
    <div class="gantt-viewport${state.planMode === 'pan' ? ' pan-mode' : ''}" data-plan-viewport data-ui-allow-overlap="The frozen task navigator intentionally overlays timeline content that has scrolled behind it inside this bounded canvas" tabindex="0" aria-label="Interactive plan timeline. Scroll horizontally or use the timeline controls to pan and zoom.">
      <div class="gantt" role="treegrid" aria-label="${esc(model.display_name)} task plan">
        <div class="gbounds">${gridLines}</div>
        <div class="grow gtools"><div class="glabel"><span class="plan-column-title">Tasks</span><span class="plan-column-estimate">Est. lines</span><button type="button" class="plan-nav-resizer" data-plan-nav-resizer role="separator" aria-orientation="vertical" aria-valuemin="260" aria-valuemax="520" aria-valuenow="${state.planNavigatorWidth}" aria-label="Resize task navigator"></button></div><div class="gtrack">${toolbar}</div></div>
        <div class="grow gaxis"><div class="glabel muted">Plan</div><div class="gtrack"><span class="plan-axis-title">Estimated lines of code</span><div class="plan-axis-ticks">${axisTicks}${unsizedAxis}</div></div></div>
        ${groups.map((group) => releaseHead(group) + group.rows.map(taskRow).join('')).join('')}
      </div>
    </div>
    <div class="plan-minimap-row" aria-label="Timeline overview">
      <div class="plan-minimap-label">Timeline overview</div>
      <div class="plan-minimap" data-plan-minimap role="scrollbar" tabindex="0" aria-label="Timeline horizontal position" aria-orientation="horizontal" aria-valuemin="0" aria-valuemax="100" aria-valuenow="0">
        <div class="plan-minimap-bars">${rows.filter((row) => !row.isParent).map((row) => `<i data-minimap-task="${esc(row.task.task_id)}" class="${row.isUnsized ? 'unsized ' : ''}${esc(row.task.status)}${row.task.task_id === state.planSelectedTaskId ? ' selected' : ''}" style="left:${row.isUnsized ? unsizedLeft : pctOf(row.start)};width:${row.isUnsized ? unsizedWidth : pctOf(row.width)}"></i>`).join('')}</div>
        <div class="plan-minimap-thumb" data-plan-minimap-thumb></div>
      </div>
    </div>
  </div>`;

  const selectionTray = planSelectionTray(selectedRow, releaseById, admin);

  const contextHeader = `<section class="plan-context" data-ui-region="plan-context">
    <div class="plan-identity"><h1>${destinationLink('Plan', '#/plan')}</h1><span class="plan-slash" aria-hidden="true">/</span>${projectPicker(projects, repoId, (id) => `#/plan/${id}`, 'plan')}</div>
    <div class="plan-total-progress"><div><strong>${locN(doneLoc)}</strong><span> lines done</span></div><progress max="${Math.max(total, 1)}" value="${doneLoc}"></progress><div><strong>${locN(total)}</strong><span> lines planned</span>${unsizedCount ? `<small>${unsizedCount} ${unsizedCount === 1 ? 'job' : 'jobs'} not estimated</small>` : ''}</div></div>
    <div class="plan-context-actions"><a href="#/decisions/${esc(repoId)}">Decisions →</a>${admin ? `<button class="btn" type="button" data-plan-feedback>${planIcon('message-plus')}Ask for a change</button>` : ''}${requestControl}</div>
  </section>`;

  main.innerHTML = `${contextHeader}${requestNotice}${elaborationNotice}
    ${!model.releases.length && !model.tasks.length ? `<div data-ui-region="plan-primary">${stateBlock('empty', 'No plan yet. The agent will publish tasks and releases here once planning starts.')}</div>` : gantt}
    ${model.tasks_truncated ? '<p class="plan-footnote muted">Only the newest finished tasks are shown; everything stays permanently recorded.</p>' : ''}
    ${selectionTray}
    <div class="plan-tooltip" id="plan-tooltip" role="tooltip" hidden></div>`;
  bind(main);
  bindProjectPicker(main);
  bindMoveButtons(main, model);
  bindPlanWorkspace(main, model, { groups, total, unsizedCount, sizedShare, releaseById, admin });
  $('[data-plan-feedback]', main)?.addEventListener('click', () => openFeedbackDialog(repoId));
  if (admin) bindGanttDrag(main, model);
});

function openFeedbackDialog(repoId) {
  document.getElementById('feedback-dialog')?.remove();
  const returnFocus = document.activeElement;
  const dlg = document.createElement('dialog');
  dlg.id = 'feedback-dialog';
  dlg.innerHTML = `<div class="dialog-head"><div><h2>Ask for a change</h2><p class="muted">Your request becomes a task in this plan.</p></div><button class="dialog-close" type="button" data-dialog-close aria-label="Close">${planIcon('x')}</button></div>
    <form id="comment-form" class="dialog-form">
      <label class="f">What you want<input name="title" required maxlength="120" placeholder="The export button gives an error"></label>
      <label class="f">Why it matters (optional)<textarea name="impact"></textarea></label>
      <div class="dialog-actions"><button class="btn btn-primary" type="submit">Send to the agent</button><button class="btn" type="button" data-dialog-close>Cancel</button></div>
    </form>`;
  document.body.appendChild(dlg);
  dlg.addEventListener('close', () => { dlg.remove(); returnFocus?.focus?.(); });
  dlg.querySelectorAll('[data-dialog-close]').forEach((button) => button.addEventListener('click', () => dlg.close()));
  $('#comment-form', dlg).addEventListener('submit', async (event) => {
    event.preventDefault();
    const data = new FormData(event.target);
    const args = { repository_id: repoId, title: data.get('title'), kind: 'user_feedback' };
    if (data.get('impact')) args.impact = data.get('impact');
    await act(event.target.querySelector('button[type=submit]'), 'task.create', args, () => { dlg.close(); return render(); });
  });
  dlg.showModal();
  $('#comment-form [name=title]', dlg).focus();
}

function openEstimateDialog(task) {
  document.getElementById('estimate-dialog')?.remove();
  const returnFocus = document.activeElement;
  const dlg = document.createElement('dialog');
  dlg.id = 'estimate-dialog';
  dlg.innerHTML = `<div class="dialog-head"><div><h2>${task.estimated_loc == null ? 'Add estimate' : 'Change estimate'}</h2><p class="muted">${esc(task.title)}</p></div><button class="dialog-close" type="button" data-dialog-close aria-label="Close">${planIcon('x')}</button></div>
    <form id="estimate-form" class="dialog-form">
      <label class="f">Estimated code and test lines<input name="estimated_loc" type="number" inputmode="numeric" min="1" max="1000000" step="1" required value="${task.estimated_loc == null ? '' : Number(task.estimated_loc)}" placeholder="Enter a line estimate"></label>
      <p class="muted">Include test code and fixtures. Do not turn test-running or investigation time into pretend lines; that effort needs a separate hours estimate.</p>
      <div class="dialog-actions"><button class="btn btn-primary" type="submit">Save estimate</button><button class="btn" type="button" data-dialog-close>Cancel</button></div>
    </form>`;
  document.body.appendChild(dlg);
  dlg.addEventListener('close', () => { dlg.remove(); returnFocus?.focus?.(); });
  dlg.querySelectorAll('[data-dialog-close]').forEach((button) => button.addEventListener('click', () => dlg.close()));
  $('#estimate-form', dlg).addEventListener('submit', async (event) => {
    event.preventDefault();
    const value = Number(new FormData(event.target).get('estimated_loc'));
    if (!Number.isInteger(value) || value < 1 || value > 1_000_000) return;
    if (value === task.estimated_loc) { dlg.close(); return; }
    await act(event.target.querySelector('button[type=submit]'), 'task.update', { task_id: task.task_id, estimated_loc: value }, () => { dlg.close(); return render(); });
  });
  dlg.showModal();
  const input = $('#estimate-form [name=estimated_loc]', dlg);
  input.focus(); input.select();
}

function syncPlanElaboration(root, model, task, initiator = null) {
  task.elaboration_needed = true;
  if (!(model.elaboration_requests || []).some((request) => request.task_id === task.task_id)) {
    model.elaboration_requests = [...(model.elaboration_requests || []), {
      task_id: task.task_id, title: task.title, status: task.status, kind: task.kind,
    }];
  }
  root.querySelectorAll(`[data-elaboration-mark="${CSS.escape(task.task_id)}"]`).forEach((mark) => {
    mark.innerHTML = badge('elaboration needed', 'warn');
  });
  root.querySelectorAll(`[data-elaborate-task="${CSS.escape(task.task_id)}"]`).forEach((button) => {
    const explanation = `A clearer explanation has been requested for ${task.title}`;
    button.disabled = false;
    button.setAttribute('aria-disabled', 'true');
    button.setAttribute('aria-label', explanation);
    button.title = explanation;
    button.innerHTML = `${planIcon('message-plus')}<span class="plan-elaborate-label">Requested</span>`;
  });
  root.querySelectorAll(`[data-hover-task="${CSS.escape(task.task_id)}"]`).forEach((bar) => {
    bar.dataset.hoverElaboration = 'true';
  });
  const notice = $('[data-plan-elaboration-notice]', root);
  if (notice) {
    const count = model.elaboration_requests.length;
    notice.hidden = false;
    notice.textContent = `${count} ${count === 1 ? 'task needs' : 'tasks need'} a clearer explanation. Agents see these requests whenever they read or update the plan.`;
  }
  initiator?.focus?.({ preventScroll: true });
}

function bindPlanElaborationButtons(root, model) {
  root.querySelectorAll('[data-elaborate-task]').forEach((button) => {
    if (button.dataset.elaborationBound === 'true') return;
    button.dataset.elaborationBound = 'true';
    button.addEventListener('click', async () => {
      const task = model.tasks.find((item) => item.task_id === button.dataset.elaborateTask);
      if (!task || task.elaboration_needed) return;
      button.disabled = true;
      try {
        await api('task.update', { task_id: task.task_id, elaboration_needed: true }, false);
        syncPlanElaboration(root, model, task, button);
      } catch (error) {
        button.disabled = false;
        toast(`Could not request elaboration: ${error.message}`, 'bad');
      }
    });
  });
}

function bindPlanSelectionTray(root, model, layout) {
  const tray = $('.plan-selection', root);
  if (!tray || tray.dataset.trayBound === 'true') return;
  tray.dataset.trayBound = 'true';
  bind(tray);
  bindMoveButtons(tray, model);
  bindPlanElaborationButtons(root, model);
  tray.querySelectorAll('[data-resize-task]').forEach((button) => button.addEventListener('click', () => {
    const task = model.tasks.find((item) => item.task_id === button.dataset.resizeTask);
    if (task) openEstimateDialog(task);
  }));
  $('[data-plan-selection-toggle]', tray)?.addEventListener('click', (event) => {
    state.planSelectionCollapsed = !state.planSelectionCollapsed;
    tray.classList.toggle('collapsed', state.planSelectionCollapsed);
    const button = event.currentTarget;
    button.setAttribute('aria-expanded', String(!state.planSelectionCollapsed));
    button.setAttribute('aria-label', `${state.planSelectionCollapsed ? 'Expand' : 'Collapse'} selected task details`);
    button.innerHTML = planIcon(state.planSelectionCollapsed ? 'chevron-up' : 'chevron-down');
    button.focus({ preventScroll: true });
  });
}

function updatePlanSelection(root, model, layout, taskId) {
  const selectedRow = layout.groups.flatMap((group) => group.rows).find((row) => row.task.task_id === taskId);
  if (!selectedRow) return;
  state.planSelectedTaskId = taskId;
  state.planSelectionCollapsed = false;
  root.querySelectorAll('.gtask.selected').forEach((row) => row.classList.remove('selected'));
  root.querySelectorAll('.gbar.selected').forEach((bar) => bar.classList.remove('selected'));
  root.querySelectorAll('[data-select-task]').forEach((control) => control.setAttribute('aria-pressed', String(control.dataset.selectTask === taskId)));
  root.querySelector(`[data-task-row="${CSS.escape(taskId)}"]`)?.classList.add('selected');
  root.querySelectorAll(`[data-select-task="${CSS.escape(taskId)}"]`).forEach((control) => {
    if (control.classList.contains('gbar')) control.classList.add('selected');
  });
  root.querySelectorAll('[data-minimap-task]').forEach((mark) => mark.classList.toggle('selected', mark.dataset.minimapTask === taskId));
  const nextTray = planSelectionTray(selectedRow, layout.releaseById, layout.admin);
  const currentTray = $('.plan-selection', root);
  if (currentTray) currentTray.outerHTML = nextTray;
  else $('#plan-tooltip', root)?.insertAdjacentHTML('beforebegin', nextTray);
  bindPlanSelectionTray(root, model, layout);
  const row = root.querySelector(`[data-task-row="${CSS.escape(taskId)}"]`);
  const viewport = $('[data-plan-viewport]', root);
  const tray = $('.plan-selection:not(.collapsed)', root);
  if (row && viewport) {
    const rowBox = row.getBoundingClientRect(); const viewportBox = viewport.getBoundingClientRect(); const trayBox = tray?.getBoundingClientRect();
    const safeTop = viewportBox.top + 90;
    const safeBottom = Math.min(viewportBox.bottom - 8, trayBox && trayBox.top > viewportBox.top ? trayBox.top - 8 : viewportBox.bottom - 8);
    if (rowBox.top < safeTop) viewport.scrollTop = Math.max(0, viewport.scrollTop - (safeTop - rowBox.top));
    else if (rowBox.bottom > safeBottom) viewport.scrollTop += rowBox.bottom - safeBottom;
  }
  row?.querySelector('.plan-task-select')?.focus({ preventScroll: true });
}

function syncPlanCollapsedRows(root, layout) {
  for (const group of layout.groups) {
    const groupRows = new Map(group.rows.map((row) => [row.task.task_id, row]));
    for (const row of group.rows) {
      let parent = groupRows.get(row.task.parent_task_id); let hidden = false;
      while (parent) {
        if (state.collapsed.has(parent.task.task_id)) { hidden = true; break; }
        parent = groupRows.get(parent.task.parent_task_id);
      }
      row.hidden = hidden;
      root.querySelector(`[data-task-row="${CSS.escape(row.task.task_id)}"]`)?.classList.toggle('ghidden', hidden);
      if (!row.isParent) continue;
      const button = root.querySelector(`[data-collapse="${CSS.escape(row.task.task_id)}"]`);
      if (!button) continue;
      const collapsed = state.collapsed.has(row.task.task_id);
      button.setAttribute('aria-expanded', String(!collapsed));
      button.setAttribute('aria-label', `${collapsed ? 'Show' : 'Hide'} subtasks of ${row.task.title}`);
      button.title = collapsed ? 'Show subtasks' : 'Hide subtasks';
      button.innerHTML = planIcon(collapsed ? 'chevron-right' : 'chevron-down');
    }
  }
}

function bindPlanWorkspace(root, model, layout) {
  const workspace = $('.plan-workspace', root);
  const viewport = $('[data-plan-viewport]', root);
  if (!workspace || !viewport) return;
  const minimap = $('[data-plan-minimap]', root);
  const thumb = $('[data-plan-minimap-thumb]', root);
  const tooltip = $('#plan-tooltip', root);
  const byId = new Map(model.tasks.map((task) => [task.task_id, task]));
  const total = layout.total;
  bindPlanSelectionTray(root, model, layout);
  bindPlanElaborationButtons(root, model);

  const navigatorWidth = () => (state.planNavigatorCollapsed ? 0 : Math.min(state.planNavigatorWidth, Math.max(180, viewport.clientWidth - 180)));
  const visibleChartWidth = () => Math.max(240, viewport.clientWidth - navigatorWidth());
  const currentChartWidth = () => Number.parseFloat(getComputedStyle(workspace).getPropertyValue('--chart-width')) || planCanvasWidth(total);
  const updateMinimap = () => {
    if (!minimap || !thumb) return;
    const contentWidth = Math.max(1, currentChartWidth());
    const visibleWidth = Math.min(contentWidth, visibleChartWidth());
    const maxScroll = Math.max(0, contentWidth - visibleWidth);
    const left = Math.max(0, Math.min(maxScroll, viewport.scrollLeft));
    const widthPercent = Math.max(6, (visibleWidth / contentWidth) * 100);
    const leftPercent = maxScroll ? (left / maxScroll) * (100 - widthPercent) : 0;
    thumb.style.width = `${Math.min(100, widthPercent)}%`;
    thumb.style.left = `${leftPercent}%`;
    const value = maxScroll ? Math.round((left / maxScroll) * 100) : 0;
    minimap.setAttribute('aria-valuenow', String(value));
  };
  const applyScale = (preserveCenter = true) => {
    workspace.style.setProperty('--glabel', `${navigatorWidth()}px`);
    const oldWidth = currentChartWidth();
    const visible = visibleChartWidth();
    const center = oldWidth ? (viewport.scrollLeft + visible / 2) / oldWidth : 0;
    const width = Math.max(240, state.planFit ? visible : planCanvasWidth(total));
    workspace.style.setProperty('--chart-width', `${Math.round(width)}px`);
    const output = $('#plan-zoom-value', root);
    if (output) output.value = state.planFit ? 'Fit' : `${Math.round(state.planZoom * 100)}%`;
    if (preserveCenter) viewport.scrollLeft = state.planFit ? 0 : Math.max(0, center * width - visible / 2);
    state.planScrollLeft = viewport.scrollLeft;
    updateMinimap();
  };

  applyScale(false);
  viewport.scrollLeft = state.planFit ? 0 : state.planScrollLeft;
  viewport.scrollTop = state.planScrollTop;
  updateMinimap();
  viewport.addEventListener('scroll', () => {
    state.planScrollLeft = viewport.scrollLeft;
    state.planScrollTop = viewport.scrollTop;
    updateMinimap();
  }, { passive: true });
  new ResizeObserver(() => applyScale(false)).observe(viewport);

  root.querySelectorAll('[data-plan-mode]').forEach((button) => button.addEventListener('click', () => {
    state.planMode = button.dataset.planMode;
    viewport.classList.toggle('pan-mode', state.planMode === 'pan');
    root.querySelectorAll('[data-plan-mode]').forEach((peer) => {
      const active = peer.dataset.planMode === state.planMode;
      peer.classList.toggle('active', active);
      peer.setAttribute('aria-pressed', String(active));
    });
    viewport.focus();
  }));
  root.querySelectorAll('[data-plan-zoom]').forEach((button) => button.addEventListener('click', () => {
    const action = button.dataset.planZoom;
    if (action === 'fit') state.planFit = true;
    else {
      const levels = [0.5, 0.75, 1, 1.25, 1.5, 2, 2.5, 3];
      const current = state.planFit ? 1 : state.planZoom;
      const index = levels.reduce((best, value, i) => Math.abs(value - current) < Math.abs(levels[best] - current) ? i : best, 0);
      state.planZoom = levels[Math.max(0, Math.min(levels.length - 1, index + (action === 'in' ? 1 : -1)))];
      state.planFit = false;
    }
    applyScale();
    button.focus();
  }));
  root.querySelectorAll('[data-plan-nav-toggle]').forEach((button) => button.addEventListener('click', () => {
    state.planNavigatorCollapsed = !state.planNavigatorCollapsed;
    workspace.classList.toggle('navigator-collapsed', state.planNavigatorCollapsed);
    button.setAttribute('aria-pressed', String(state.planNavigatorCollapsed));
    button.title = `${state.planNavigatorCollapsed ? 'Show' : 'Hide'} task navigator`;
    button.innerHTML = `${planIcon(state.planNavigatorCollapsed ? 'layout-sidebar-left-expand' : 'layout-sidebar-left-collapse')}<span class="sr-only">${state.planNavigatorCollapsed ? 'Show' : 'Hide'} task navigator</span>`;
    applyScale(false);
    button.focus({ preventScroll: true });
  }));

  root.querySelectorAll('[data-select-task]').forEach((control) => {
    const select = () => {
      if (state.planMode === 'pan') return;
      const taskId = control.dataset.selectTask;
      updatePlanSelection(root, model, layout, taskId);
    };
    control.addEventListener('click', (event) => { if (!event.target.closest('[data-resize-handle]')) select(); });
    if (control.getAttribute('role') === 'button') control.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); select(); }
    });
  });
  root.querySelectorAll('[data-collapse]').forEach((button) => button.addEventListener('click', () => {
    const taskId = button.dataset.collapse;
    if (state.collapsed.has(taskId)) state.collapsed.delete(taskId); else state.collapsed.add(taskId);
    syncPlanCollapsedRows(root, layout);
    button.focus({ preventScroll: true });
  }));

  const navResizer = $('[data-plan-nav-resizer]', root);
  if (navResizer) {
    const setNavigatorWidth = (width) => {
      state.planNavigatorWidth = Math.round(Math.max(260, Math.min(520, width)));
      workspace.style.setProperty('--glabel', `${navigatorWidth()}px`);
      navResizer.setAttribute('aria-valuenow', String(navigatorWidth()));
      if (state.planFit) applyScale(false); else updateMinimap();
    };
    navResizer.addEventListener('keydown', (event) => {
      if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return;
      event.preventDefault();
      setNavigatorWidth(state.planNavigatorWidth + (event.key === 'ArrowRight' ? 12 : -12));
    });
    navResizer.addEventListener('pointerdown', (event) => {
      event.preventDefault();
      const startX = event.clientX; const startWidth = state.planNavigatorWidth;
      navResizer.setPointerCapture(event.pointerId);
      const move = (next) => setNavigatorWidth(startWidth + next.clientX - startX);
      const end = () => {
        navResizer.removeEventListener('pointermove', move);
        navResizer.removeEventListener('pointerup', end);
        navResizer.removeEventListener('pointercancel', end);
      };
      navResizer.addEventListener('pointermove', move);
      navResizer.addEventListener('pointerup', end);
      navResizer.addEventListener('pointercancel', end);
    });
  }

  let panning = null;
  viewport.addEventListener('pointerdown', (event) => {
    if (state.planMode !== 'pan' || event.button !== 0 || event.target.closest('button,a,input,select,textarea,.glabel')) return;
    panning = { id: event.pointerId, x: event.clientX, y: event.clientY, left: viewport.scrollLeft, top: viewport.scrollTop };
    viewport.setPointerCapture(event.pointerId);
    viewport.classList.add('is-panning');
    event.preventDefault();
  });
  viewport.addEventListener('pointermove', (event) => {
    if (!panning || panning.id !== event.pointerId) return;
    viewport.scrollLeft = panning.left - (event.clientX - panning.x);
    viewport.scrollTop = panning.top - (event.clientY - panning.y);
  });
  const stopPan = (event) => {
    if (!panning || panning.id !== event.pointerId) return;
    panning = null; viewport.classList.remove('is-panning');
  };
  viewport.addEventListener('pointerup', stopPan);
  viewport.addEventListener('pointercancel', stopPan);
  viewport.addEventListener('keydown', (event) => {
    if (event.target !== viewport) return;
    if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
      event.preventDefault(); viewport.scrollBy({ left: event.key === 'ArrowRight' ? 96 : -96, behavior: 'smooth' });
    }
    if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
      event.preventDefault(); viewport.scrollBy({ top: event.key === 'ArrowDown' ? 64 : -64, behavior: 'smooth' });
    }
  });

  if (minimap && thumb) {
    minimap.addEventListener('click', (event) => {
      if (event.target === thumb) return;
      const box = minimap.getBoundingClientRect();
      const ratio = Math.max(0, Math.min(1, (event.clientX - box.left) / box.width));
      viewport.scrollLeft = ratio * currentChartWidth() - visibleChartWidth() / 2;
      viewport.focus();
    });
    minimap.addEventListener('keydown', (event) => {
      if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight' && event.key !== 'Home' && event.key !== 'End') return;
      event.preventDefault();
      if (event.key === 'Home') viewport.scrollLeft = 0;
      else if (event.key === 'End') viewport.scrollLeft = viewport.scrollWidth;
      else viewport.scrollLeft += event.key === 'ArrowRight' ? 96 : -96;
    });
    thumb.addEventListener('pointerdown', (event) => {
      event.preventDefault(); event.stopPropagation();
      const box = minimap.getBoundingClientRect();
      const startX = event.clientX; const startScroll = viewport.scrollLeft;
      const maxScroll = Math.max(0, currentChartWidth() - visibleChartWidth());
      const travel = Math.max(1, box.width - thumb.getBoundingClientRect().width);
      thumb.setPointerCapture(event.pointerId);
      const move = (next) => { viewport.scrollLeft = startScroll + ((next.clientX - startX) / travel) * maxScroll; };
      const end = () => {
        thumb.removeEventListener('pointermove', move);
        thumb.removeEventListener('pointerup', end);
        thumb.removeEventListener('pointercancel', end);
      };
      thumb.addEventListener('pointermove', move);
      thumb.addEventListener('pointerup', end);
      thumb.addEventListener('pointercancel', end);
    });
  }

  root.querySelectorAll('[data-hover-task]').forEach((bar) => {
    const show = () => {
      if (!tooltip) return;
      tooltip.innerHTML = `<strong>${esc(bar.dataset.hoverTitle)}</strong>${planBadge(bar.dataset.hoverStatus)}${bar.dataset.hoverElaboration === 'true' ? badge('elaboration needed', 'warn') : ''}<span>${esc(bar.dataset.hoverLoc)}</span><span>${esc(bar.dataset.hoverProgress)}</span>`;
      tooltip.hidden = false;
      requestAnimationFrame(() => {
        const anchor = bar.getBoundingClientRect(); const box = tooltip.getBoundingClientRect();
        const left = Math.max(8, Math.min(window.innerWidth - box.width - 8, anchor.left + anchor.width / 2 - box.width / 2));
        const above = anchor.top - box.height - 10;
        tooltip.style.left = `${left}px`;
        tooltip.style.top = `${above > 8 ? above : Math.min(window.innerHeight - box.height - 8, anchor.bottom + 10)}px`;
      });
    };
    const hide = () => { if (tooltip) tooltip.hidden = true; };
    bar.addEventListener('pointerenter', show); bar.addEventListener('pointerleave', hide);
    bar.addEventListener('focus', show); bar.addEventListener('blur', hide);
  });

  root.querySelectorAll('[data-resize-handle]').forEach((handle) => {
    handle.addEventListener('pointerdown', (event) => {
      event.preventDefault(); event.stopPropagation();
      const task = byId.get(handle.dataset.resizeHandle);
      const bar = handle.closest('.gbar'); const track = handle.closest('.gtrack');
      if (!task || !bar || !track || !task.estimated_loc || !total) return;
      const startX = event.clientX; const original = task.estimated_loc;
      const unitsPerPixel = total / Math.max(1, track.getBoundingClientRect().width);
      const step = original >= 5000 ? 100 : original >= 500 ? 10 : 1;
      let nextValue = original; let cancelled = false;
      const readout = document.createElement('span'); readout.className = 'plan-resize-readout'; bar.appendChild(readout);
      const preview = (clientX) => {
        nextValue = Math.max(1, Math.round((original + (clientX - startX) * unitsPerPixel) / step) * step);
        bar.style.width = `${(nextValue / total) * (layout.sizedShare ?? 1) * 100}%`;
        readout.textContent = `~${locN(nextValue)} lines`;
      };
      const cleanup = () => {
        window.removeEventListener('keydown', cancelOnEscape);
        handle.removeEventListener('pointermove', move);
        handle.removeEventListener('pointerup', finish);
        handle.removeEventListener('pointercancel', cancel);
        readout.remove(); bar.classList.remove('is-resizing');
      };
      const cancel = () => { cancelled = true; cleanup(); render(); };
      const cancelOnEscape = (keyEvent) => { if (keyEvent.key === 'Escape') { keyEvent.preventDefault(); cancel(); } };
      const move = (next) => preview(next.clientX);
      const finish = async () => {
        cleanup();
        if (cancelled || nextValue === original) { render(); return; }
        try {
          await api('task.update', { task_id: task.task_id, estimated_loc: nextValue });
          toast(`estimate updated to ${locN(nextValue)} lines`, 'ok');
        } catch (error) { toast(`resize failed: ${error.message}`, 'bad'); }
        render();
      };
      handle.setPointerCapture(event.pointerId); bar.classList.add('is-resizing'); preview(startX);
      window.addEventListener('keydown', cancelOnEscape);
      handle.addEventListener('pointermove', move);
      handle.addEventListener('pointerup', finish);
      handle.addEventListener('pointercancel', cancel);
    });
  });
}

// Pop-up move/reorder — the touch and accessibility path beside drag-and-drop.
function openMoveDialog(task, model) {
  document.getElementById('move-dialog')?.remove();
  const open = model.releases.filter((r) => r.status !== 'delivered');
  const current = model.releases.find((r) => r.release_id === task.release_id);
  const dlg = document.createElement('dialog');
  dlg.id = 'move-dialog';
  dlg.innerHTML = `<h2>Move: ${esc(task.title)}</h2>
    <p class="muted">Now in ${esc(current ? current.name : 'not scheduled yet')}.</p>
    <form id="move-form" class="inline">
      <label class="f">move to<select name="release_id">${open.map((r) => `<option value="${esc(r.release_id)}"${r.release_id === task.release_id ? ' selected' : ''}>${esc(r.name)}</option>`).join('')}<option value=""${task.release_id ? '' : ' selected'}>not scheduled yet (backlog)</option></select></label>
      <label class="f">put first<input type="checkbox" name="first"></label>
      <div class="actions" style="flex-basis:100%">
        <button class="btn" type="submit">Move</button>
        <button class="btn" type="button" id="move-cancel">Cancel</button>
      </div>
    </form>`;
  document.body.appendChild(dlg);
  dlg.addEventListener('close', () => dlg.remove());
  $('#move-cancel', dlg).addEventListener('click', () => dlg.close());
  $('#move-form', dlg).addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const fd = new FormData(ev.target);
    const target = fd.get('release_id') || null;
    const args = { task_id: task.task_id };
    if (target !== (task.release_id || null)) args.release_id = target;
    if (fd.get('first') === 'on') args.position = 0;
    if (!('release_id' in args) && !('position' in args)) { dlg.close(); return; }
    await act(ev.target.querySelector('button[type=submit]'), 'task.update', args, () => { dlg.close(); return render(); });
  });
  dlg.showModal();
}
function bindMoveButtons(root, model) {
  root.querySelectorAll('[data-move-task]').forEach((btn) => {
    if (btn.dataset.moveBound === 'true') return;
    btn.dataset.moveBound = 'true';
    btn.addEventListener('click', () => {
      const t = model.tasks.find((x) => x.task_id === btn.dataset.moveTask);
      if (t) openMoveDialog(t, model);
    });
  });
}

// Native HTML5 drag-and-drop: reorder within a release, move across releases
// (drop between rows), or drop on a release header to append to it.
let dragTaskId = null;
function bindGanttDrag(root, model) {
  const known = new Set(model.releases.map((r) => r.release_id));
  const groupOf = (t) => (t.release_id && known.has(t.release_id) ? t.release_id : null);
  const byId = new Map(model.tasks.map((t) => [t.task_id, t]));
  const clearMarks = () => root.querySelectorAll('.gdrop-before,.gdrop-after,.gdrop-into').forEach((el) => el.classList.remove('gdrop-before', 'gdrop-after', 'gdrop-into'));
  const move = async (args) => {
    try { await api('task.update', args); toast('task moved', 'ok'); render(); }
    catch (e) { toast(`move failed: ${e.message}`, 'bad'); }
  };
  root.querySelectorAll('[data-drag-task]').forEach((row) => {
    row.addEventListener('dragstart', (ev) => {
      dragTaskId = row.dataset.dragTask;
      ev.dataTransfer.effectAllowed = 'move';
      ev.dataTransfer.setData('text/plain', dragTaskId);
    });
    row.addEventListener('dragend', () => { dragTaskId = null; clearMarks(); });
  });
  root.querySelectorAll('[data-task-row]').forEach((row) => {
    row.addEventListener('dragover', (ev) => {
      if (!dragTaskId || dragTaskId === row.dataset.taskRow) return;
      ev.preventDefault();
      clearMarks();
      row.classList.add(ev.offsetY < row.offsetHeight / 2 ? 'gdrop-before' : 'gdrop-after');
    });
    row.addEventListener('drop', (ev) => {
      const sourceId = ev.dataTransfer?.getData('text/plain') || dragTaskId;
      if (!sourceId || sourceId === row.dataset.taskRow) return;
      ev.preventDefault();
      const before = ev.offsetY < row.offsetHeight / 2;
      const target = byId.get(row.dataset.taskRow);
      const dragged = byId.get(sourceId);
      clearMarks(); dragTaskId = null;
      if (!target || !dragged) return;
      const siblings = model.tasks
        .filter((t) => (t.parent_task_id || null) === (target.parent_task_id || null) && groupOf(t) === groupOf(target) && t.task_id !== dragged.task_id)
        .sort((a, b) => (a.position - b.position) || (a.seq - b.seq));
      const idx = siblings.findIndex((t) => t.task_id === target.task_id);
      const args = { task_id: dragged.task_id, position: Math.max(0, before ? idx : idx + 1) };
      if ((target.release_id || null) !== (dragged.release_id || null)) args.release_id = target.release_id || null;
      if ((target.parent_task_id || null) !== (dragged.parent_task_id || null)) args.parent_task_id = target.parent_task_id || null;
      move(args);
    });
  });
  root.querySelectorAll('[data-drop-release]').forEach((head) => {
    head.addEventListener('dragover', (ev) => { if (!dragTaskId) return; ev.preventDefault(); clearMarks(); head.classList.add('gdrop-into'); });
    head.addEventListener('dragleave', () => head.classList.remove('gdrop-into'));
    head.addEventListener('drop', (ev) => {
      const sourceId = ev.dataTransfer?.getData('text/plain') || dragTaskId;
      if (!sourceId) return;
      ev.preventDefault();
      const dragged = byId.get(sourceId);
      const target = head.dataset.dropRelease || null;
      clearMarks(); dragTaskId = null;
      if (!dragged) return;
      if ((dragged.release_id || null) === target && !dragged.parent_task_id) return;
      const args = { task_id: dragged.task_id, release_id: target };
      if (dragged.parent_task_id) args.parent_task_id = null;
      move(args);
    });
  });
}

// --- Decisions -------------------------------------------------------------
const ASPECTS = ['all', 'ui', 'architecture', 'algorithms', 'business_logic', 'data', 'testing', 'deployment', 'security', 'performance', 'process', 'other'];
const paragraphs = (text) => String(text).split(/\n+/).filter(Boolean).map((p) => `<p>${esc(p)}</p>`).join('');
const viewDecisions = guard(async (repoId) => {
  main.innerHTML = `${pageHeading('Decisions', '#/decisions')}${skeleton(5)}`;
  const aspect = state.decisionAspect !== 'all' ? { aspect: state.decisionAspect } : {};
  const searching = !!state.decisionQuery;
  const resultRequest = searching
    ? api('decision.search', { repository_id: repoId, query: state.decisionQuery, n: state.decisionLimit, ...aspect })
    : api('decision.tail', { repository_id: repoId, n: state.decisionLimit, ...aspect, ...(state.decisionBefore ? { before_seq: state.decisionBefore } : {}) });
  const [result, projectList] = await Promise.all([
    resultRequest,
    workspace.active ? {} : api('plan.overview', {}),
  ]);
  const projects = [...(projectList.repositories || []), {
    repository_id: repoId, display_name: result.display_name || 'Current project',
  }];
  const entries = searching ? result.decisions : [...result.decisions].reverse();
  const card = (d) => {
    const head = `<strong>${esc(d.title)}</strong> ${badge(d.aspect.replace('_', ' '))}${d.ref ? ` <span class="muted mono">${esc(d.ref)}</span>` : ''} <span class="muted">${ago(d.created_at)}</span>`;
    if (d.superseded_by) return `<details class="decision superseded"><summary>${head} ${badge('superseded')}</summary>${paragraphs(d.body)}</details>`;
    return `<div class="decision">${head}${paragraphs(d.body)}</div>`;
  };
  const story = !searching && !state.decisionBefore
    ? `<details class="story"><summary>The story so far</summary>${result.summary ? paragraphs(result.summary.body) : '<p class="muted">No summary yet.</p>'}</details>` : '';
  const emptyText = searching ? 'Nothing found for that search.'
    : state.decisionAspect !== 'all' ? `No ${state.decisionAspect.replace('_', ' ')} decisions yet.`
      : 'No decisions yet. The agent records its choices here as it works.';
  main.innerHTML = `<div class="repository-context"><h1>${destinationLink('Decisions', '#/decisions')}</h1><span class="context-slash" aria-hidden="true">/</span>${projectPicker(projects, repoId, (id) => `#/decisions/${id}`, 'decisions')}</div>
    ${story}
    <form class="inline" id="decision-search"><label class="f">search every decision<input name="q" value="${esc(state.decisionQuery)}" placeholder="e.g. why exports are files"></label><button class="btn" type="submit">Search</button>${searching || state.decisionBefore ? '<button class="btn" type="button" id="decisions-latest">Show latest</button>' : ''}</form>
    <div class="segwrap">${seg(ASPECTS, state.decisionAspect, 'decision-aspect', (o) => o.replace('_', ' '))}</div>
    ${entries.length ? entries.map(card).join('') : stateBlock('empty', emptyText)}
    ${!searching && result.has_more ? '<p><button class="btn" id="decisions-older">Show older decisions</button></p>' : ''}`;
  bindProjectPicker(main);
  bindSeg(main, 'decision-aspect', (a) => { state.decisionAspect = a; state.decisionBefore = null; render(); });
  $('#decision-search').addEventListener('submit', (ev) => {
    ev.preventDefault();
    state.decisionQuery = String(new FormData(ev.target).get('q') || '').trim();
    state.decisionBefore = null;
    render();
  });
  $('#decisions-latest')?.addEventListener('click', () => { state.decisionQuery = ''; state.decisionBefore = null; render(); });
  $('#decisions-older')?.addEventListener('click', () => {
    if (entries.length) state.decisionBefore = entries[entries.length - 1].seq;
    render();
  });
});

// --- Router ----------------------------------------------------------------
const workspace = window.DevCoordinatorWorkspace.create({ api, esc, identity: () => state.who });
window.addEventListener('resize', () => {
  const composer = $('#evidence-composer', main);
  if (!composer || composer.hidden) return;
  openEvidenceComposer(undefined, false);
});
async function render() {
  closeGlossaryDialog?.();
  closeActiveProjectPicker?.(false);
  viewAbort?.abort();
  viewAbort = new AbortController();
  const signal = viewAbort.signal;
  let route;
  try { route = await workspace.resolve(signal); }
  catch (error) {
    if (signal.aborted || error.code === 'stale' || error.code === 'unauthenticated') return;
    if (['test_evidence_expired', 'test_evidence_not_found'].includes(error.code)) {
      main.innerHTML = currentDestinationHeading() + stateBlock('empty', 'No visual evidence is available for this run. It may have expired.');
      return;
    }
    main.innerHTML = currentDestinationHeading() + stateBlock(error.code === 'permission_denied' ? 'denied' : 'error', error.message);
    return;
  }
  if (!route || signal.aborted) return;
  const { view, arg } = route;
  main.classList.toggle('plan-page', view === 'plan' && !!arg);
  main.classList.toggle('glossary-page', view === 'glossary');
  main.classList.toggle('usage-page', view === 'usage' && !!arg);
  main.classList.toggle('progress-page', view === 'progress' && !!arg);
  main.classList.toggle('health-page', view === 'health');
  main.classList.toggle('deployments-page', view === 'deployments' && !arg);
  main.classList.toggle('test-evidence-page', view === 'tests' && !!arg);
  main.classList.toggle('tests-collection-page', view === 'tests' && !arg);
  document.body.classList.toggle('plan-shell', view === 'plan' && !!arg);
  document.body.classList.toggle('evidence-shell', view === 'tests' && !!arg);
  if (!(view === 'tests' && arg) && state.evidenceRunId) {
    evidenceCanvasSession?.observer?.disconnect(); evidenceCanvasSession = null;
    resetEvidenceImages(); state.evidenceRunId = null; state.evidenceData = null;
    state.evidenceSteps = []; state.evidenceRun = null;
  }
  document.querySelectorAll('#nav a').forEach((a) => a.classList.toggle('active', a.dataset.view === view));
  setBanner('');
  if (route.empty) {
    main.innerHTML = currentDestinationHeading() + stateBlock('empty', 'No repositories visible to you.');
    return;
  }
  if (view === 'deployments') return arg ? viewDeployment(arg) : viewDeployments();
  if (view === 'plan') return arg ? viewPlan(arg) : viewPlanPicker('plan');
  if (view === 'progress') return arg ? viewProgress(arg) : viewProgressRepositories();
  if (view === 'usage') return arg ? viewCodexUsage(arg) : viewCodexUsageRepositories();
  if (view === 'decisions') return arg ? viewDecisions(arg) : viewPlanPicker('decisions');
  if (view === 'glossary') return viewGlossary();
  if (view === 'tests') return viewTests(arg || null, route.settings);
  if (view === 'health') return viewHealth(arg);
  if (view === 'bugs') return viewBugs();
  if (view === 'admin') return viewAdmin();
  location.hash = '#/plan';
  return undefined;
}
window.render = render;
window.addEventListener('hashchange', render);
setupTopNavigation();
{
  const [, initialView] = (location.hash || '#/deployments').slice(1).split('/');
  document.querySelectorAll('#nav a').forEach((anchor) => anchor.classList.toggle('active', anchor.dataset.view === initialView));
  main.innerHTML = `${currentDestinationHeading()}${skeleton()}`;
}
(async () => {
  try {
    state.who = await api('user.whoami', {});
    $('#who-email').textContent = state.who.identity || 'local';
    $('#nav-admin').hidden = !state.who.administrator;
  } catch (e) { if (e.code !== 'unauthenticated') setBanner(`Cannot reach the coordinator: ${e.message}`); }
  render();
})();
