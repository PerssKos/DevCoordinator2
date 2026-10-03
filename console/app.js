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
  planExplicitVisibleTaskIds: new Set(),
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
  evidenceSource: 'test',
  sketchRepositoryId: null,
  sketchDetail: null,
  sketchGalleryData: null,
  sketchSelectionMode: 'multiple',
  sketchDrawerSet: null,
  sketchDrawerSketchId: null,
  sketchDrawerDetail: null,
  sketchDrawerLoading: false,
  sketchSetGroup: null,
  sketchSetDetails: new Map(),
  sketchGalleryOffset: 0,
  sketchGalleryHasMore: false,
  sketchGalleryLoadingMore: false,
  sketchGalleryLoadToken: 0,
  collapsedDeploymentRepositories: new Set(),
  collapsedDeploymentWorkers: new Set(),
  deploymentUsageResolutions: new Map(),
  planVisibilitySeededRepositoryId: null,
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
function pct(n) { return window.DevCoordinatorI18n.percent(n == null ? null : Number(n) / 100, { minimumFractionDigits: 1 }); }
function ago(iso) {
  if (!iso || !Number.isFinite(Date.parse(iso))) return '—';
  const seconds = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  const [unit, divisor] = seconds < 90 ? ['second', 1] : seconds < 5400 ? ['minute', 60] : seconds < 172800 ? ['hour', 3600] : ['day', 86400];
  return new Intl.RelativeTimeFormat(window.DevCoordinatorI18n.locale, { numeric: 'always', style: 'narrow' }).format(-Math.round(seconds / divisor), unit);
}
function until(iso) {
  if (!iso || !Number.isFinite(Date.parse(iso))) return '—';
  const seconds = (Date.parse(iso) - Date.now()) / 1000;
  if (seconds <= 0) return window.DevCoordinatorI18n.t('common.expired');
  const [unit, divisor] = seconds < 90 ? ['second', 1] : seconds < 5400 ? ['minute', 60] : seconds < 172800 ? ['hour', 3600] : ['day', 86400];
  return new Intl.RelativeTimeFormat(window.DevCoordinatorI18n.locale, { numeric: 'always', style: 'narrow' }).format(Math.round(seconds / divisor), unit);
}
function badge(text, kind) {
  const cls = kind || ({ running: 'ok', healthy: 'ok', passed: 'ok', stopped: '', degraded: 'warn', unhealthy: 'bad', failed: 'bad', 'timed-out': 'bad', cancelled: '', interrupted: 'warn', superseded: '', applying: 'warn', unknown: '', none: '' }[text] ?? '');
  const key = 'status_' + String(text).replaceAll(/[^a-zA-Z0-9]/g, '_');
  const known = ["running","healthy","passed","stopped","degraded","unhealthy","failed","timed-out","cancelled","interrupted","superseded","applying","unknown","none","observed","host","critical","warning","active","complete","partial","unavailable","pending","planned","in_progress","done","dropped","requested","delivered","your request","administrator","viewer","operator","open","resolved","deleted","checked","expired","draft","approved","deprecated","keep","reject","undecided","Earlier visual run","preview","elaboration needed"].includes(text);
  return `<span class="badge ${cls}">${known ? window.DevCoordinatorI18n.markup('common.' + key) : esc(text)}</span>`;
}

function destinationLink(label, href) {
  const keys = {"Deployments":"destination_deployments","Plan":"destination_plan","Progress":"destination_progress","Codex Usage":"destination_usage","Performance":"destination_performance","Decisions":"destination_decisions","Sketches":"destination_sketches","Glossary":"destination_glossary","Tests":"destination_tests","Health":"destination_health","Bugs":"destination_bugs","Administration":"destination_admin","Shared glossary":"destination_shared_glossary"};
  return `<a class="destination-link" href="${esc(href)}">${keys[label] ? window.DevCoordinatorI18n.markup('common.' + keys[label]) : esc(label)}</a>`;
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
    <span class="project-picker-menu" id="${esc(menuId)}" role="menu" aria-label="Projects" data-project-picker-menu hidden data-i18n-attrs='{"aria-label":"common.projects_04e2a9"}'>
      ${options.map((project) => `<a role="menuitem" href="${esc(hrefFor(project.repository_id))}"${project.repository_id === currentId ? ' aria-current="page"' : ''}>${esc(project.display_name)}${project.repository_id === currentId ? "<span class=\"project-picker-selected\"><span data-i18n=\"common.current_e0d1b6\">Current</span></span>" : ''}</a>`).join('')}
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
  return `<svg class="spark" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" aria-label="trend" data-i18n-attrs='{"aria-label":"common.trend_2ab022"}'><polyline fill="none" stroke="#4c8dff" stroke-width="1.5" points="${pts}"/></svg>`;
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
function chart(points, fmt, label, chartLabels = {}) {
  if (!points || points.length < 2) {
    return `<div class="chartbox"><div class="chartmeta"><span>${window.DevCoordinatorI18n.computedMarkup(() => window.DevCoordinatorI18n.label(label))}</span></div><div class="notice muted"><span data-i18n="common.no_history_for_this_window_yet_samples_accumulat_f6c3ff">No history for this window yet. Samples accumulate while the coordinator runs.</span></div></div>`;
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
    <div class="chartmeta"><span>${window.DevCoordinatorI18n.computedMarkup(() => window.DevCoordinatorI18n.label(label))}</span><span><span class="muted">${esc(chartLabels.scale || 'scale')} 0–${fmt(maxV)} · ${esc(chartLabels.now || 'now')}</span> <strong>${fmt(last.avg)}</strong></span></div>
    <svg class="chart" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" aria-label="${esc(window.DevCoordinatorI18n.label(label))} history">${grid}<polygon points="${band}" class="band"/><polyline points="${avg}" class="line" fill="none"/></svg>
    <div class="chartaxis"><span>${esc(minuteLabel(points[0].minute))}</span><span class="muted"><span data-i18n="common.min_max_band_average_line_5fe15d">min–max band, average line</span></span><span>${esc(minuteLabel(last.minute))}</span></div>
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
  el.className = `toast ${kind}`;
  if (typeof text === 'function') window.DevCoordinatorI18n.bind(el, text);
  else el.textContent = text;
  $('#toasts').appendChild(el);
  setTimeout(() => el.remove(), 6000);
}

class ApiError extends Error {
  constructor(code, message) {
    super(); this.code = code; this.originalMessage = message;
    const key = ['permission_denied','params_invalid','busy','daemon_unavailable','network'].includes(code) ? code
      : code.endsWith('_not_found') ? 'not_found' : code.includes('expired') ? 'expired'
        : code.includes('tampered') ? 'tampered' : code.includes('conflict') ? 'conflict' : 'request';
    Object.defineProperty(this, 'message', { get: () => window.DevCoordinatorI18n.t('common.error_' + key) + ' [' + code + ']' + (message ? ': ' + message : '') });
  }
}
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

function setBanner(text) { const b = $('#banner'); b.hidden = !text; window.DevCoordinatorI18n.bind(b, typeof text === 'function' ? text : () => (text || '')); }
function skeleton(rows = 4) { return Array.from({ length: rows }, () => '<div class="skeleton"></div>').join(''); }
function stateBlock(kind, text) {
  const value = () => typeof text === 'function' ? text() : text;
  if (kind === 'denied') return `<div class="notice denied">${window.DevCoordinatorI18n.computedMarkup(() => window.DevCoordinatorI18n.t('common.permissionDenied', { message: value() }))}</div>`;
  if (kind === 'error') return `<div class="notice"><strong><span data-i18n="common.could_not_load_32ccbd">Could not load.</span></strong> ${window.DevCoordinatorI18n.computedMarkup(value)} <button class="btn btn-small" onclick="render()"><span data-i18n="common.retry_942087">Retry</span></button></div>`;
  return `<div class="notice muted">${window.DevCoordinatorI18n.computedMarkup(value)}</div>`;
}
function currentDestinationHeading() {
  const [, view, arg] = (location.hash || '#/plan').split('?')[0].slice(1).split('/');
  const destinations = {
    deployments: ['Deployments', '#/deployments'], plan: ['Plan', '#/plan'],
    progress: ['Progress', '#/progress'], usage: ['Codex Usage', '#/usage'], performance: ['Performance', '#/performance'],
    decisions: ['Decisions', '#/decisions'], sketches: ['Sketches', '#/sketches'],
    glossary: ['Glossary', '#/glossary'],
    tests: ['Tests', '#/tests'], health: ['Health', '#/health'],
    bugs: ['Bugs', '#/bugs'], admin: ['Administration', '#/admin'], requests: ['Feature requests', '#/requests'],
  };
  const destination = destinations[view] || destinations.deployments;
  return pageHeading(destination[0], destination[1], view === 'health' && arg === 'containers' ? 'Containers' : '');
}
function guard(fn) {
  return async (...args) => {
    try { return await fn(...args); } catch (error) {
      if (error.code === 'stale') return null; // another view took over
      if (error.code === 'permission_denied') main.innerHTML = currentDestinationHeading() + stateBlock('denied', () => error.message);
      else if (['test_evidence_expired', 'test_evidence_not_found'].includes(error.code)) main.innerHTML = currentDestinationHeading() + stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_visual_evidence_is_available_for_this_run_it__4f1873"));
      else if (error.code !== 'unauthenticated') main.innerHTML = currentDestinationHeading() + stateBlock('error', () => error.message);
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
    toast(() => window.DevCoordinatorI18n.t("common.value1_failed_value2_23ce24", {value1: command, value2: error.message}), 'bad');
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
    window.DevCoordinatorI18n.text(toggle, open ? 'shell.closeNavigation' : 'shell.open_navigation_0ed77f', {}, 'aria-label');
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
  const blocked = !canOperate ? ' disabled aria-disabled="true" data-i18n-attrs=\'{"title":"deployments.operatorRequired"}\'' : deploymentState === 'applying' ? ' disabled aria-disabled="true" data-i18n-attrs=\'{"title":"deployments.refreshAfterApply"}\'' : '';
  return ['start', 'stop', 'restart'].map((a) => `<button class="${cls}" data-cmd="deployment.${a}" data-args='${esc(JSON.stringify(args))}'${blocked}>${window.DevCoordinatorI18n.markup('deployments.action_' + a)}</button>`).join('');
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
    <div class="deployment-record-status" aria-label="Deployment status" data-i18n-attrs='{"aria-label":"deployments.deployment_status_a62786"}'>${badge(deployment.state)} ${health} ${deployment.observed_only ? badge('observed') : ''}</div>
    <div class="deployment-record-body"><dl class="deployment-record-facts deployment-record-endpoint">
      <div><dt><span data-i18n="deployments.domain_79fa33">Domain</span></dt><dd><span class="deployment-domain">${domain}</span>${admin ? ` <button class="btn btn-small deployment-domain-edit" data-edit-domain="${esc(deployment.deployment_id)}" ${window.DevCoordinatorI18n.computedAttribute("aria-label", () => window.DevCoordinatorI18n.t("deployments.editDomain", { name: deployment.name, source: deployment.source }))}><span data-i18n="deployments.edit_262121">edit</span></button>` : ''}</dd></div>
      <div><dt><span data-i18n="deployments.port_72e9a5">Port</span></dt><dd>${deployment.route_port ?? '—'}</dd></div>
    </dl>
    <dl class="deployment-record-facts deployment-record-runtime">
      <div><dt><span data-i18n="deployments.generation_40536d">Generation</span></dt><dd>${deployment.current_generation ?? '—'}</dd></div>
      <div><dt><span data-i18n="deployments.updated_3a5ecc">Updated</span></dt><dd>${window.DevCoordinatorI18n.computedMarkup(() => ago(deployment.updated_at))}</dd></div>
    </dl>
    <div class="deployment-record-actions"><span><span data-i18n="deployments.actions_ff8059">Actions</span></span><div class="actions">${lifecycleButtons(deployment.deployment_id, null, 'btn btn-small', deployment.state)}
      ${!deployment.observed_only && admin ? `<button class="btn btn-small" data-cmd="deployment.apply" data-args='${esc(JSON.stringify({ deployment_id: deployment.deployment_id }))}'${deployment.state === 'applying' ? ' disabled aria-disabled="true" data-i18n-attrs=\'{"title":"deployments.applyRunning"}\'' : ''}><span data-i18n="deployments.apply_97a5e4">apply</span></button>` : ''}</div></div></div>
  </article>`;
}

function dashboardUsageDisplay(usage, canOperate, sourceError = null, resolving = false) {
  if (!canOperate) return { value: 'Operator access required', note: '' };
  const totals = usage?.totals || usage;
  const coverage = usage?.coverage;
  if (totals?.total_tokens != null) {
    return {
      value: `${compactNumber(totals.total_tokens)} tokens · ${Number(totals.model_requests || 0).toLocaleString(window.DevCoordinatorI18n.locale)} requests`,
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
    progressValue = `${Math.round(progress.planned_lines_done / progress.planned_lines_total * 100)}% · ${Number(progress.planned_lines_done).toLocaleString(window.DevCoordinatorI18n.locale)} of ${Number(progress.planned_lines_total).toLocaleString(window.DevCoordinatorI18n.locale)} lines`;
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
      <span class="deployment-repository-count">${count} ${count === 1 ? window.DevCoordinatorI18n.markup("deployments.deployment_aee50b") : window.DevCoordinatorI18n.markup("deployments.deployments_01366d")}</span>
      <strong class="deployment-repository-status ${overall.kind}">${esc(overall.text)}</strong>
      <button class="deployment-collapse-toggle deployment-repository-toggle" type="button" data-deployment-repository-toggle="${esc(collapseKey)}" data-ui-continuation-anchor aria-expanded="${!collapsed}" aria-controls="${bodyId}" aria-label="${collapsed ? 'Expand' : 'Collapse'} ${esc(repositoryName)} repository">${planIcon(collapsed ? 'chevron-right' : 'chevron-down')}</button>
    </header>
    <div class="deployment-repository-body" id="${bodyId}"${collapsed ? ' hidden' : ''}><div class="deployment-repository-summary" aria-label="${esc(`${repositoryName} repository summary`)}">${summary}</div>
    <section class="deployment-workers${workersCollapsed ? ' collapsed' : ''}" aria-labelledby="${workersId}-title"><header class="deployment-workers-head"><div class="deployment-workers-title"><strong id="${workersId}-title"><span data-i18n="deployments.workers_7a1ec9">Workers</span></strong><span>${count}</span></div><button class="deployment-collapse-toggle deployment-workers-toggle" type="button" data-deployment-workers-toggle="${esc(collapseKey)}" data-ui-continuation-anchor aria-expanded="${!workersCollapsed}" aria-controls="${workersId}" aria-label="${workersCollapsed ? 'Expand' : 'Collapse'} ${count} ${count === 1 ? 'worker' : 'workers'} for ${esc(repositoryName)}"><span class="deployment-workers-toggle-label">${workersCollapsed ? window.DevCoordinatorI18n.markup("deployments.expand_workers_c57c47") : window.DevCoordinatorI18n.markup("deployments.collapse_workers_7a9ec3")}</span>${planIcon(workersCollapsed ? 'chevron-right' : 'chevron-down')}</button></header>
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
    window.DevCoordinatorI18n.bind(button, () => window.DevCoordinatorI18n.t(collapsed ? 'common.expandRepository' : 'common.collapseRepository', { name: section?.querySelector('h2')?.textContent || '' }), "aria-label");
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
    window.DevCoordinatorI18n.bind(button, () => window.DevCoordinatorI18n.t(collapsed ? 'common.expandWorkers' : 'common.collapseWorkers', { count, name: repositoryName }), "aria-label");
    button.innerHTML = `<span class="deployment-workers-toggle-label">${(collapsed ? window.DevCoordinatorI18n.markup("deployments.expand_workers_c57c47") : window.DevCoordinatorI18n.markup("deployments.collapse_workers_7a9ec3"))}</span>${planIcon(collapsed ? 'chevron-right' : 'chevron-down')}`;
  }));
}

const viewDeployments = guard(async () => {
  main.innerHTML = `<section class="deployments-dashboard">${pageHeading('Deployments', '#/deployments')}${skeleton(8)}</section>`;
  if (workspace.active) {
    const result = await api('deployment.list', {});
    const deployments = result.deployments.filter(workspace.matches);
    main.innerHTML = `<section class="deployments-dashboard">${pageHeading('Deployments', '#/deployments')}${deployments.length ? `<div class="deployment-records">${deployments.map((deployment) => deploymentRecord(deployment, state.who?.administrator)).join('')}</div>` : stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_deployments_have_been_applied_for_this_reposi_720c5a"))}</section>`;
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
  if (!deployments.length) { main.innerHTML = `${pageHeading('Deployments', '#/deployments')}${stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_deployments_have_been_applied_yet_f5fd2a"))}`; return; }
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
      catch (e) { toast(() => e.message, 'bad'); components = []; }
    }
    componentOptions = (components || []).map((c) => `<option>${esc(c.name)}</option>`).join('');
  }
  const dlg = document.createElement('dialog');
  dlg.id = 'domain-dialog';
  dlg.innerHTML = `<h2>${window.DevCoordinatorI18n.markup("deployments.domain_value1_value2_ee9534", {value1: d.name, value2: d.source})}</h2>
    ${d.repository_name ? `<p class="muted">${esc(d.repository_name)}</p>` : ''}
    <form id="domain-form" class="inline">
      <label class="f"><span data-i18n="deployments.domain_label_206b66">domain label</span><input name="domain" value="${esc(d.domain || '')}" placeholder="my-app" pattern="[a-z0-9]([a-z0-9-]*[a-z0-9])?" title="lowercase DNS label" autofocus data-i18n-attrs='{"placeholder":"deployments.my_app_4c9a75","title":"deployments.lowercase_dns_label_3dbe17"}'></label>
      ${needsTarget ? `<label class="f"><span data-i18n="deployments.host_port_228285">host port</span><input name="port" type="number" min="1" max="65535" required></label><label class="f"><span data-i18n="deployments.component_6985ca">component</span><select name="component">${componentOptions}</select></label>` : ''}
      <label class="f"><span data-i18n="deployments.public_no_sign_in_014496">public (no sign-in)</span><input type="checkbox" name="public" ${d.public ? 'checked' : ''}></label>
      <div class="actions" style="flex-basis:100%">
        <button class="btn" type="submit"><span data-i18n="deployments.save_domain_080d60">Save domain</span></button>
        ${d.domain ? "<button class=\"btn\" type=\"button\" id=\"domain-clear\"><span data-i18n=\"deployments.remove_routed_domain_43d229\">Remove routed domain</span></button>" : ''}
        <button class="btn" type="button" id="domain-cancel"><span data-i18n="deployments.cancel_19766e">Cancel</span></button>
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
    await act(ev.currentTarget, 'deployment.set_domain', { deployment_id: d.deployment_id, domain: null }, () => { dlg.close(); return render(); });
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
      <td class="wrap"><strong>${esc(c.name)}</strong>${c.display_name ? `<div class="muted">${esc(c.display_name)}</div>` : ''}<div class="muted">${esc(c.type)}${(obs ? window.DevCoordinatorI18n.markup("deployments.exact_recorded_container_a9b79f") : (c.owned ? '' : window.DevCoordinatorI18n.markup("deployments.external_855e84")))}</div></td>
      <td>${badge(c.state)} ${badge(c.health)}</td><td>${c.generation ?? '—'}</td><td>${c.port ?? '—'}</td><td>${c.restarts ?? '—'}</td>
      <td class="wrap mono">${esc(c.binding?.kind || '')} ${esc((c.binding?.identity || '').slice(0, 24))}</td>
      <td class="wrap">${c.last_error ? `<span class="badge bad">${esc(c.last_error)}</span>` : ''}</td>
      <td class="actions">${controllable(c) ? lifecycleButtons(id, c.name, 'btn btn-small', d.state) : ''}
        ${c.owned || obs ? `<button class="btn btn-small" data-logs="${esc(c.name)}"><span data-i18n="deployments.logs_98f38f">logs</span></button>` : ''}</td></tr>`;
    const serviceRows = (c.services || []).map((service) => `<tr class="service-row" data-compose-service="${esc(`${c.name}/${service.name}`)}">
      <td class="wrap"><strong>↳ ${esc(service.name)}</strong><div class="muted">${window.DevCoordinatorI18n.markup("deployments.compose_value3_service_87ae79", {value3: service.role})}</div></td>
      <td>${badge(service.state)}</td><td>${c.generation ?? '—'}</td><td>—</td><td>—</td>
      <td class="muted">${window.DevCoordinatorI18n.markup("deployments.containerCount", { count: service.containers ?? 0 })}</td><td>—</td>
      <td class="actions">${service.independent ? lifecycleButtons(id, `${c.name}/${service.name}`, 'btn btn-small', d.state) : ''}</td></tr>`);
    return [componentRow, ...serviceRows];
  }).join('');
  main.innerHTML = `${pageHeading('Deployments', '#/deployments', `${d.name}@${d.source}`, ` ${badge(d.state)} ${d.health && d.health !== d.state ? badge(d.health) : ''}`)}
    <p class="muted">${d.repository_name ? `<span data-i18n="deployments.repositoryLabel"></span>: <strong>${esc(d.repository_name)}</strong> ` : ''}<span class="mono">${esc(d.repository_id || '')}</span></p>
    <div class="grid"><div class="tile"><div class="k"><span data-i18n="deployments.domain_79fa33">Domain</span> ${admin ? "<button class=\"btn btn-small\" id=\"edit-domain\"><span data-i18n=\"deployments.edit_262121\">edit</span></button>" : ''}</div><div class="v">${d.domain ? esc(d.domain) : '—'}</div>${d.public ? "<div class=\"muted\"><span data-i18n=\"deployments.public_no_sign_in_014496\">public (no sign-in)</span></div>" : ''}</div><div class="tile"><div class="k"><span data-i18n="deployments.route_port_2c38fa">Route port</span></div><div class="v">${d.route_port ?? '—'}</div></div><div class="tile"><div class="k"><span data-i18n="deployments.generation_40536d">Generation</span></div><div class="v">${d.current_generation ?? '—'}${d.previous_generation ? ` <span class="muted">${window.DevCoordinatorI18n.markup("deployments.prev_value1_8d66fe", {value1: d.previous_generation})}</span>` : ''}</div></div><div class="tile"><div class="k"><span data-i18n="deployments.expires_f6725f">Expires</span></div><div class="v">${(d.ttl_expires_at ? esc(d.ttl_expires_at) : window.DevCoordinatorI18n.markup("deployments.never_6497e4"))}</div></div></div>
    ${obs ? "<p class=\"notice muted\"><span data-i18n=\"deployments.imported_from_the_live_host_start_stop_restart_a_cd3182\">Imported from the live host. Start, stop, restart, and logs act on the exact recorded containers. Configuration changes (apply, rollback, remove) require adopting the stack through repository configuration.</span></p>" : ''}
    ${d.state === 'applying' ? "<p class=\"notice\"><span data-i18n=\"deployments.apply_is_still_running_closing_this_page_does_no_83d405\">Apply is still running. Closing this page does not cancel it. Refresh status after it finishes; other lifecycle actions stay unavailable meanwhile.</span></p>" : ''}
    <div class="actions" style="margin:12px 0">${lifecycleButtons(id, null, 'btn', d.state)}
      ${!obs && admin ? `<button class="btn" data-cmd="deployment.apply" data-args='${esc(JSON.stringify({ deployment_id: id }))}'${d.state === 'applying' ? ' disabled aria-disabled="true" data-i18n-attrs=\'{"title":"deployments.applyRunning"}\'' : ''}><span data-i18n="deployments.apply_97a5e4">apply</span></button><button class="btn" data-cmd="deployment.rollback" data-args='${esc(JSON.stringify({ deployment_id: id }))}'${d.state === 'applying' ? ' disabled aria-disabled="true" data-i18n-attrs=\'{"title":"deployments.applyRunning"}\'' : ''}><span data-i18n="deployments.rollback_da2548">rollback</span></button><button class="btn btn-danger" data-cmd="deployment.remove" data-args='${esc(JSON.stringify({ deployment_id: id, delete_data: false }))}'${d.state === 'applying' ? ' disabled aria-disabled="true" data-i18n-attrs=\'{"title":"deployments.applyRunning"}\'' : ''}><span data-i18n="deployments.remove_deployment_keep_data_b46030">Remove deployment — keep data</span></button><button class="btn btn-danger" data-cmd="deployment.remove" data-args='${esc(JSON.stringify({ deployment_id: id, delete_data: true }))}'${d.state === 'applying' ? ' disabled aria-disabled="true" data-i18n-attrs=\'{"title":"deployments.applyRunning"}\'' : ''}><span data-i18n="deployments.remove_deployment_and_delete_data_9fe36e">Remove deployment and delete data</span></button>` : ''}</div>
    <h2><span data-i18n="deployments.components_a150ce">Components</span></h2><div class="tablewrap"><table><thead><tr><th><span data-i18n="deployments.component_ce54f0">Component</span></th><th><span data-i18n="deployments.state_a3b50c">State</span></th><th><span data-i18n="deployments.gen_ca0aad">Gen</span></th><th><span data-i18n="deployments.port_72e9a5">Port</span></th><th><span data-i18n="deployments.restarts_4fdc8b">Restarts</span></th><th><span data-i18n="deployments.binding_164c69">Binding</span></th><th><span data-i18n="deployments.error_54a0e8">Error</span></th><th><span data-i18n="deployments.actions_ff8059">Actions</span></th></tr></thead><tbody>${rows}</tbody></table></div>
    <div id="logs"></div><h2>Usage ${seg(Object.keys(RANGES), state.usageRange, 'usage-range')}</h2><div id="usage">${skeleton(2)}</div>`;
  bind(main);
  $('#edit-domain')?.addEventListener('click', () => openDomainDialog(d));
  bindSeg(main, 'usage-range', (r) => { state.usageRange = r; render(); });
  main.querySelectorAll('[data-logs]').forEach((btn) => btn.addEventListener('click', async () => {
    btn.disabled = true;
    try { const r = await api('deployment.logs', { deployment_id: id, component: btn.dataset.logs, tail_lines: 200 }, false); $('#logs').innerHTML = `<h2>${window.DevCoordinatorI18n.markup("deployments.logs_value1_306798", {value1: btn.dataset.logs})}</h2><h3><span data-i18n="deployments.untrusted_log_text_192942">Untrusted log text</span></h3><pre class="log" aria-label="Untrusted log text" data-i18n-attrs='{"aria-label":"deployments.untrusted_log_text_192942"}'>${esc(r.tail || '(empty)')}</pre>${r.log_path ? `<p class="muted mono">${esc(r.log_path)}</p>` : ''}`; }
    catch (e) { toast(() => e.message, 'bad'); } finally { btn.disabled = false; }
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
    $('#usage').innerHTML = usage.length ? usage.join('') : "<p class=\"muted\"><span data-i18n=\"deployments.no_measured_components_6f4369\">No measured components.</span></p>";
  } catch (e) {
    if (e.code === 'stale') return;
    const el = $('#usage');
    if (el) el.innerHTML = stateBlock(e.code === 'permission_denied' ? 'denied' : 'error', () => e.message);
  }
});

// --- Tests ---------------------------------------------------------------
const TEST_TIERS = ['development', 'pre-merge', 'release'];
function testTierLabel(tier) {
  const key = { development: 'development_21b6a7', 'pre-merge': 'pre_merge_175f55', release: 'release_e020e3' }[tier];
  return window.DevCoordinatorI18n.t(key ? 'tests.' + key : 'common.unavailable_ca1844');
}
function testCapacityAdjustment(adjustment) {
  if (!adjustment) return "<p class=\"muted\"><span data-i18n=\"tests.no_adjustment_recorded_45c3ac\">No adjustment recorded.</span></p>";
  const reason = {
    underused_saturated_epoch: 'Increased after a saturated, underused epoch',
    sustained_pressure: 'Decreased after sustained host pressure',
    administrator_cap_changed: 'Administrator maximum changed',
  }[adjustment.reason] || healthLabel(adjustment.reason, {});
  return `<dl class="test-capacity-adjustment">
    <div><dt><span data-i18n="tests.reason_f81ab8">Reason</span></dt><dd>${esc(reason)}</dd></div>
    <div><dt><span data-i18n="tests.change_c0bf75">Change</span></dt><dd>${esc(adjustment.previous_capacity)} → ${esc(adjustment.new_capacity)}</dd></div>
    <div><dt><span data-i18n="tests.cpu_p95_c8e5f2">CPU p95</span></dt><dd>${window.DevCoordinatorI18n.computedMarkup(() => pct(adjustment.p95_cpu_percent))}</dd></div>
    <div><dt><span data-i18n="tests.memory_p95_9e9ebe">Memory p95</span></dt><dd>${window.DevCoordinatorI18n.computedMarkup(() => pct(adjustment.p95_memory_percent))}</dd></div>
    <div><dt><span data-i18n="tests.saturated_ca7078">Saturated</span></dt><dd>${adjustment.saturation_fraction == null ? '—' : pct(Number(adjustment.saturation_fraction) * 100)}</dd></div>
    <div><dt><span data-i18n="tests.epoch_fff7a2">Epoch</span></dt><dd>${adjustment.epoch_seconds == null ? '—' : `${esc(adjustment.epoch_seconds)}s`}</dd></div>
    <div><dt><span data-i18n="tests.recorded_c7175f">Recorded</span></dt><dd>${adjustment.at ? esc(ago(adjustment.at)) : '—'}</dd></div>
  </dl>`;
}
function openTestCapacityDialog(capacity, opener) {
  document.getElementById('test-capacity-dialog')?.remove();
  if (opener?.closest('details')) opener.closest('details').open = false;
  const dlg = document.createElement('dialog');
  dlg.id = 'test-capacity-dialog';
  dlg.innerHTML = `<div class="dialog-head"><h2><span data-i18n="tests.test_capacity_408011">Test capacity</span></h2><button class="dialog-close" type="button" aria-label="Close capacity settings" data-i18n-attrs='{"aria-label":"tests.close_capacity_settings_2eb13f"}'>×</button></div>
    <dl class="test-capacity-facts">
      <div><dt><span data-i18n="tests.auto_capacity_8c8e2f">Auto capacity</span></dt><dd>${esc(capacity.learned_capacity)}</dd></div>
      <div><dt><span data-i18n="tests.effective_a4f3df">Effective</span></dt><dd>${esc(capacity.effective_capacity)}</dd></div>
      <div><dt><span data-i18n="tests.maximum_66c9ab">Maximum</span></dt><dd>${capacity.cap == null ? window.DevCoordinatorI18n.markup("tests.none_dc937b") : esc(capacity.cap)}</dd></div>
      <div><dt><span data-i18n="tests.active_923406">Active</span></dt><dd>${esc(capacity.active)}</dd></div>
      <div><dt><span data-i18n="tests.waiting_6e293a">Waiting</span></dt><dd>${esc(capacity.waiting)}</dd></div>
      <div><dt><span data-i18n="tests.admission_480730">Admission</span></dt><dd>${capacity.paused ? badge('paused', 'warn') : badge('open', 'ok')}</dd></div>
    </dl>
    <h3><span data-i18n="tests.last_adjustment_9a2109">Last adjustment</span></h3>${testCapacityAdjustment(capacity.last_adjustment)}
    <form id="test-capacity-form" class="dialog-form">
      <label class="f"><span data-i18n="tests.maximum_parallel_checks_95fd96">Maximum parallel checks</span><input name="cap" type="number" min="1" step="1" inputmode="numeric" value="${capacity.cap == null ? '' : esc(capacity.cap)}" placeholder="No maximum" data-i18n-attrs='{"placeholder":"tests.no_maximum_b5cfdf"}'></label>
      <div class="dialog-actions"><button class="btn" type="button" id="test-capacity-cancel"><span data-i18n="tests.cancel_19766e">Cancel</span></button>${capacity.cap == null ? '' : "<button class=\"btn\" type=\"button\" id=\"test-capacity-clear\"><span data-i18n=\"tests.clear_maximum_c51f6c\">Clear maximum</span></button>"}<button class="btn btn-primary" type="submit"><span data-i18n="tests.save_maximum_edd496">Save maximum</span></button></div>
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
      input.setCustomValidity(window.DevCoordinatorI18n.t("common.enter_a_whole_number_of_at_least_1_1387e8"));
      event.target.reportValidity(); return;
    }
    const button = event.submitter || event.target.querySelector('button[type=submit]');
    await act(button, 'test.capacity.set', { cap }, saveAndReturn);
  });
  $('#test-capacity-clear', dlg)?.addEventListener('click', async (event) => {
    await act(event.currentTarget, 'test.capacity.set', { cap: null }, saveAndReturn);
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
function logEntryLabel(entry, run) {
  const ref = logSelector(entry);
  const check = run?.checks?.find((check) => check.name === ref.check)?.display_name || ref.check;
  const stream = ref.stream === 'stderr' ? 'Error output' : 'Standard output';
  if (ref.phase === 'executor') return `Test runner · ${stream}`;
  if (ref.phase === 'discovery') return `${check} · Discovery · ${stream}`;
  if (ref.phase === 'case') return `${check} · ${ref.case || 'Cases'} · ${stream}`;
  if (ref.phase === 'fixture' || ref.phase === 'cleanup') return `${check} · ${ref.case} · ${ref.phase === 'fixture' ? 'Database setup' : 'Cleanup'} · ${stream}`;
  return `${check || 'Check'} · ${stream}`;
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
    return `<section class="log-result${formatted.format ? ' structured' : ''}" data-log-row-anchor="${esc(logRowAnchor(row))}"><h3>${esc(coordinates)}${esc(count)}${formatted.format ? `<span class="log-format-badge">${esc(formatted.format)}</span>` : ''}</h3><pre class="log" aria-label="Untrusted log text" data-i18n-attrs='{"aria-label":"tests.untrusted_log_text_192942"}'><code>${highlightLogText(formatted.text)}</code></pre></section>`;
  }).join('')}</div>`;
}
function renderLogError(message) {
  return `<div class="notice"><strong><span data-i18n="tests.could_not_load_log_09c7b7">Could not load log.</span></strong> ${esc(message)}</div>`;
}
async function openTestLogsDialog(run, retention, opener) {
  document.getElementById('test-logs-dialog')?.remove();
  const dlg = document.createElement('dialog');
  dlg.id = 'test-logs-dialog';
  dlg.innerHTML = `<div class="dialog-head"><h2><span data-i18n="tests.test_logs_55496c">Test logs</span></h2><button class="dialog-close" type="button" aria-label="Close test logs" data-i18n-attrs='{"aria-label":"tests.close_test_logs_326894"}'>×</button></div>
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
    window.DevCoordinatorI18n.bind(latest, () => (entry.complete ? window.DevCoordinatorI18n.t("common.jump_to_latest_867524") : window.DevCoordinatorI18n.t("common.refresh_latest_4ae8e9")));
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
      ? `<div class="log-boundary">${(reader.operation === 'tail' ? window.DevCoordinatorI18n.markup("tests.start_of_output_85c6f8") : window.DevCoordinatorI18n.markup("tests.all_results_shown_ad3d94"))}</div>` : '';
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
      window.DevCoordinatorI18n.bind($('#test-log-view-title', dlg), () => (operation === 'search' ? window.DevCoordinatorI18n.t("common.searching_log_909a9f") : (operation === 'failure_context' ? window.DevCoordinatorI18n.t("common.finding_likely_failure_000092") : window.DevCoordinatorI18n.t("common.loading_latest_output_535532"))));
      $('#test-log-view-status', dlg).textContent = '';
    } else {
      target.querySelector('.log-page-error')?.remove();
      const indicator = document.createElement('div');
      indicator.className = 'log-loading-more'; indicator.id = 'test-log-loading-more';
      window.DevCoordinatorI18n.bind(indicator, () => (operation === 'tail' ? window.DevCoordinatorI18n.t("common.loading_earlier_output_96e879") : window.DevCoordinatorI18n.t("common.loading_more_results_d16d8b")));
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
        target.innerHTML = `${renderLogError(error.message)}<button class="btn" type="button" id="test-log-retry"><span data-i18n="tests.try_again_d8b839">Try again</span></button>`;
        window.DevCoordinatorI18n.text($('#test-log-view-title', dlg), "common.log_unavailable_adee0f");
        $('#test-log-view-status', dlg).textContent = '';
        $('#test-log-retry', target).addEventListener('click', () => read(operation, options, cursor, direction));
      } else {
        pagingPaused = true;
        target.querySelector('#test-log-loading-more')?.remove();
        const errorBox = document.createElement('div'); errorBox.className = 'log-page-error';
        errorBox.innerHTML = `<span>${esc(error.message)}</span><button class="btn btn-small" type="button"><span data-i18n="tests.try_again_d8b839">Try again</span></button>`;
        if (operation === 'tail') target.prepend(errorBox); else target.append(errorBox);
        window.DevCoordinatorI18n.text($('#test-log-view-status', dlg), "common.more_output_could_not_be_loaded_07793e");
        $('button', errorBox).addEventListener('click', () => {
          pagingPaused = false; read(operation, options, cursor, direction);
        });
      }
    }
  };
  const readLatest = () => read('tail', { lines: 200, max_bytes: 49152 });
  const renderCatalogue = () => {
    if (!entries.length) {
      catalogRoot.innerHTML = stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_retained_logs_for_this_run_b39d59")); return;
    }
    selectedIndex = Math.min(selectedIndex, entries.length - 1);
    catalogRoot.innerHTML = `<div class="test-log-reader">
      <div class="test-log-stream-row"><label class="f"><span data-i18n="tests.output_stream_33141d">Output stream</span><select id="test-log-stream">${entries.map((entry, index) => `<option value="${index}"${index === selectedIndex ? ' selected' : ''}>${esc(logEntryLabel(entry, run))}</option>`).join('')}</select></label>
        ${catalogCursor ? "<button class=\"btn\" type=\"button\" id=\"test-log-more\"><span data-i18n=\"tests.show_more_streams_c86d8d\">Show more streams</span></button>" : ''}</div>
      <div class="test-log-summary" id="test-log-summary"></div>
      <details class="test-log-details"><summary><span data-i18n="tests.stream_details_5afc75">Stream details</span></summary><div id="test-log-metadata"></div></details>
      <div class="test-log-toolbar">
        <form id="test-log-search" class="test-log-search"><label class="sr-only" for="test-log-search-text"><span data-i18n="tests.search_this_log_5b02ad">Search this log</span></label><input id="test-log-search-text" name="text" type="search" required maxlength="4096" placeholder="Search this log" data-i18n-attrs='{"placeholder":"tests.search_this_log_5b02ad"}'><button class="btn" type="submit"><span data-i18n="tests.search_49c266">Search</span></button></form>
        <button class="btn" type="button" data-log-read="failure_context"><span data-i18n="tests.show_likely_failure_bd1716">Show likely failure</span></button>
        <button class="btn" type="button" id="test-log-latest" hidden><span data-i18n="tests.jump_to_latest_867524">Jump to latest</span></button>
      </div>
      <section class="test-log-viewer" aria-labelledby="test-log-view-title"><div class="test-log-view-head"><div><strong id="test-log-view-title" data-ui-continuation-anchor><span data-i18n="tests.loading_latest_output_535532">Loading latest output</span></strong><span id="test-log-view-status"></span></div><span class="test-log-trust"><span data-i18n="tests.untrusted_log_output_0fb479">Untrusted log output</span></span></div>
        <div id="test-log-read-result" class="test-log-scroll" tabindex="0" aria-live="polite" aria-busy="true">${skeleton()}</div></section>
      </div>`;
    const metadata = () => {
      const entry = selectedEntry();
      $('#test-log-summary', dlg).textContent = logSummary(entry);
      $('#test-log-metadata', dlg).innerHTML = `<dl class="test-log-facts">
        <div><dt><span data-i18n="tests.bytes_4d8000">Bytes</span></dt><dd>${window.DevCoordinatorI18n.computedMarkup(() => bytes(entry.bytes))}</dd></div><div><dt><span data-i18n="tests.lines_3b26a5">Lines</span></dt><dd>${(entry.lines == null ? window.DevCoordinatorI18n.markup("tests.pending_331551") : esc(entry.lines))}</dd></div>
        <div><dt><span data-i18n="tests.complete_143b27">Complete</span></dt><dd>${(entry.complete ? window.DevCoordinatorI18n.markup("tests.yes_85a39a") : window.DevCoordinatorI18n.markup("tests.in_progress_c1f88e"))}</dd></div><div><dt><span data-i18n="tests.truncated_d9d9fc">Truncated</span></dt><dd>${(entry.truncated ? window.DevCoordinatorI18n.markup("tests.yes_85a39a") : window.DevCoordinatorI18n.markup("tests.no_1ea442"))}</dd></div>
        <div><dt><span data-i18n="tests.first_output_102b90">First output</span></dt><dd>${entry.first_byte_at ? esc(ago(entry.first_byte_at)) : '—'}</dd></div><div><dt><span data-i18n="tests.last_output_f75e77">Last output</span></dt><dd>${entry.last_byte_at ? esc(ago(entry.last_byte_at)) : '—'}</dd></div>
        <div><dt><span data-i18n="tests.sha_256_bbd07c">SHA-256</span></dt><dd class="mono">${(entry.sha256 ? esc(entry.sha256) : window.DevCoordinatorI18n.markup("tests.pending_331551"))}</dd></div><div><dt><span data-i18n="tests.expires_f6725f">Expires</span></dt><dd>${(entry.expires_at ? esc(until(entry.expires_at)) : window.DevCoordinatorI18n.markup("tests.active_923406"))}</dd></div>
        <div><dt><span data-i18n="tests.history_depth_08a139">History depth</span></dt><dd>${(entry.depth_rank == null ? window.DevCoordinatorI18n.markup("tests.active_923406") : `${esc(entry.depth_rank)} of ${esc(retention.case_depth)}`)}</dd></div>
        <div><dt><span data-i18n="tests.structured_evidence_c59ccc">Structured evidence</span></dt><dd>${(entry.structured_evidence?.available ? `${esc(entry.structured_evidence.count)} · ${esc((entry.structured_evidence.formats || []).join(', '))}` : window.DevCoordinatorI18n.markup("tests.none_dc937b"))}</dd></div>
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
      if (entries.length) { toast(() => error.message, 'bad'); return; }
      catalogRoot.innerHTML = `${renderLogError(error.message)}<button class="btn" type="button" id="test-log-catalog-retry"><span data-i18n="tests.try_again_d8b839">Try again</span></button>`;
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
  dlg.innerHTML = `<div class="dialog-head"><h2><span data-i18n="tests.log_retention_46c385">Log retention</span></h2><button class="dialog-close" type="button" aria-label="Close log retention settings" data-i18n-attrs='{"aria-label":"tests.close_log_retention_settings_b6e27a"}'>×</button></div>
    <form id="test-log-retention-form" class="dialog-form">
      <label class="f"><span data-i18n="tests.maximum_age_in_hours_e4f2b7">Maximum age in hours</span><input name="max_age_hours" autocomplete="off" type="number" min="0.0002777778" step="any" required value="${esc(hours)}"></label>
      <label class="f"><span data-i18n="tests.runs_kept_per_case_537456">Runs kept per case</span><input name="case_depth" autocomplete="off" type="number" min="1" max="65535" step="1" required value="${esc(retention.case_depth)}"></label>
      <div class="dialog-actions"><button class="btn" type="button" data-retention-cancel><span data-i18n="tests.cancel_19766e">Cancel</span></button><button class="btn btn-primary" type="submit"><span data-i18n="tests.save_and_clean_eligible_logs_2fa65b">Save and clean eligible logs</span></button></div>
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
      toast(() => window.DevCoordinatorI18n.t("common.enter_a_positive_age_and_a_whole_number_case_dep_0607ec"), 'bad'); return;
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
  composer.innerHTML = `<h2 id="evidence-compose-title"><span data-i18n="evidence.new_feedback_16ca40">New feedback</span></h2><form id="evidence-feedback-create"><label class="f"><span data-i18n="evidence.suggestion_527f6b">Suggestion</span><textarea name="body" rows="3" minlength="3" maxlength="2000" required placeholder="Describe what should change" data-i18n-attrs='{"placeholder":"evidence.describe_what_should_change_051892"}'>${esc(state.evidenceDraftBody)}</textarea></label><p id="evidence-feedback-error" role="alert" hidden></p><div class="actions"><button class="btn" type="button" data-evidence-cancel><span data-i18n="evidence.cancel_19766e">Cancel</span></button><button class="btn btn-primary" type="submit" disabled><span data-i18n="evidence.create_feedback_task_efe49d">Create feedback task</span></button></div></form>`;
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
        state.evidenceSelectedFeedbackId = result.feedback?.feedback_id || result.annotation?.annotation_id || null;
        revealEvidenceDiscussion(); redrawEvidenceCanvas(); updateEvidencePageStatus();
      }
      toast(() => window.DevCoordinatorI18n.t(result.annotation ? "evidence.annotationSaved" : "common.feedback_task_created_9a6d46"), 'ok');

    } else if (state.evidenceDraftKey === key) {
      const error = $('#evidence-feedback-error', form);
      window.DevCoordinatorI18n.bind(error, () => window.DevCoordinatorI18n.t("common.could_not_confirm_the_save_value1_your_comment_a_678e78", {value1: failure?.message || 'the request failed'}));
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
      const result = ['sketch', 'sketch-set'].includes(state.evidenceSource)
        ? await api('design.sketch.image', { repository_id: state.sketchRepositoryId, sketch_id: state.evidenceSource === 'sketch-set' ? image.image_id : state.evidenceRun.run_id, offset, max_bytes: 184320 })
        : await api('test.evidence.image', { path: run.worktree_path, run_id: run.run_id, image_id: image.image_id, offset, max_bytes: 184320 });
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
  if (result === 'passed') return "<span class=\"evidence-result ok\"><span data-i18n=\"evidence.passed_436fe7\">Passed</span></span>";
  if (result === 'attention') return "<span class=\"evidence-result warn\"><span data-i18n=\"evidence.review_aff076\">Review</span></span>";
  return "<span class=\"evidence-result bad\"><span data-i18n=\"evidence.failed_031a8f\">Failed</span></span>";
}

function renderEvidenceRail() {
  return state.evidenceSteps.map((step, index) => {
    const active = step.key === state.evidenceStepKey;
    const preview = step.variants.find((cell) => cell.screenshots?.viewport?.status === 'available')
      || step.variants.find((cell) => cell.screenshots?.full_page?.status === 'available');
    const image = preview?.screenshots?.viewport?.status === 'available'
      ? preview.screenshots.viewport : preview?.screenshots?.full_page;
    return `<button type="button" class="evidence-step${active ? ' active' : ''}" data-evidence-step="${esc(step.key)}" aria-pressed="${active}">
      <span class="evidence-step-thumb">${image ? `<img alt="" data-evidence-thumb="${esc(image.image_id)}">` : "<span><span data-i18n=\"evidence.unavailable_ca1844\">Unavailable</span></span>"}</span>
      <span class="evidence-step-copy"><strong><i>${index + 1}</i>${esc(step.label)}</strong><small>${esc(step.route)}</small><span>${step.variants.map((cell) => `<b>${(cell.viewport?.name ? esc(cell.viewport?.name) : window.DevCoordinatorI18n.markup("evidence.viewport_280503"))}</b>`).join('')} · ${step.duration ? `${(step.duration / 1000).toFixed(1)}s` : '—'}</span></span>
      ${evidenceResultMark(step.result)}
    </button>`;
  }).join('');
}

function renderEvidenceVariants(step, selectedCell) {
  if (state.evidenceSource === 'sketch-set') {
    return state.evidenceSteps.map((option) => {
      const cell = option.variants[0];
      const sketch = state.sketchGalleryData?.find((item) => item.sketch_id === cell.screenshots?.viewport?.image_id);
      const image = cell.screenshots?.viewport;
      const active = option.key === state.evidenceStepKey;
      const decision = sketch?.decision || 'undecided';
      return `<button type="button" class="evidence-variant sketch-set-variant${active ? ' active' : ''}${decision === 'keep' ? ' selected' : ''}" data-evidence-step="${esc(option.key)}" aria-pressed="${active}">
        <span>${image ? `<img alt="${esc(sketch?.title || option.label)}" data-evidence-thumb="${esc(image.image_id)}">` : `<span class="muted">${window.DevCoordinatorI18n.markup("evidence.screenshot_unavailable_b9bfa8")}</span>`}</span>
        <strong>${esc(sketch?.title || option.label)}</strong><small>${badge(decision, decision === 'keep' ? 'ok' : decision === 'reject' ? 'bad' : '')}</small>
      </button>`;
    }).join('');
  }
  return step.variants.map((cell) => {
    const image = cell.screenshots?.viewport?.status === 'available'
      ? cell.screenshots.viewport : cell.screenshots?.full_page;
    const active = cell === selectedCell;
    return `<button type="button" class="evidence-variant${active ? ' active' : ''}" data-evidence-viewport="${esc(cell.viewport?.name || '')}" aria-pressed="${active}">
      <span>${image ? `<img alt="${esc(`${step.label}, ${cell.viewport?.name || 'viewport'}`)}" data-evidence-thumb="${esc(image.image_id)}">` : "<span class=\"muted\"><span data-i18n=\"evidence.screenshot_unavailable_b9bfa8\">Screenshot unavailable</span></span>"}</span>
      <strong>${(cell.viewport?.name ? esc(cell.viewport?.name) : window.DevCoordinatorI18n.markup("evidence.viewport_91e53b"))}</strong><small>${esc(cell.viewport?.width)} × ${esc(cell.viewport?.height)}</small>
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
  const sketchDecision = ['sketch', 'sketch-set'].includes(state.evidenceSource) ? `<section class="evidence-inspector-section sketch-decision"><h2><span data-i18n="evidence.decision_640ae4">Decision</span></h2><p class="muted">${window.DevCoordinatorI18n.markup("evidence.value1_revision_value2_0cd02a", {value1: window.DevCoordinatorI18n.t("common.status_" + (state.sketchDetail?.sketch?.decision || 'undecided')), value2: state.sketchDetail?.sketch?.decision_revision ?? 0})}</p><div class="actions">${['keep','reject','undecided'].map((value) => `<button class="btn btn-small" type="button" data-sketch-decision="${value}">${(value === 'keep' ? window.DevCoordinatorI18n.markup("evidence.keep_183f00") : (value === 'reject' ? window.DevCoordinatorI18n.markup("evidence.reject_ab604a") : window.DevCoordinatorI18n.markup("evidence.undecided_00cc36")))}</button>`).join('')}</div><button class="btn btn-small" type="button" data-sketch-record><span data-i18n="evidence.view_generation_record_ce5581">View generation record</span></button></section>` : '';
  const sketchSetComment = state.evidenceSource === 'sketch-set' ? (() => {
    const group = state.sketchSetGroup;
    const selectedCount = group?.sketches.filter((item) => item.decision === 'keep').length || 0;
    const active = group?.sketches.find((item) => item.sketch_id === state.evidenceRun.run_id);
    return `<section class="evidence-inspector-section sketch-set-comment-compose"><h2>${window.DevCoordinatorI18n.markup("evidence.reviewComment")}</h2><form id="sketch-set-comment-form"><label class="f">${window.DevCoordinatorI18n.markup("evidence.attachTo")}<select name="scope"><option value="active">${active?.title ? esc(active.title) : window.DevCoordinatorI18n.markup("evidence.activeOption")}</option><option value="selected"${selectedCount ? '' : ' disabled'}>${window.DevCoordinatorI18n.markup("evidence.selectedOptions", {count:selectedCount})}</option></select></label><label class="f">${window.DevCoordinatorI18n.markup("evidence.commentLabel")}<textarea name="body" rows="4" minlength="3" maxlength="2000" required placeholder="Add a comment about this option or selected set." data-i18n-attrs='{"placeholder":"evidence.commentPlaceholder"}'></textarea></label><button class="btn btn-primary" type="submit">${window.DevCoordinatorI18n.markup("evidence.postComment")}</button></form></section>`;
  })() : '';
  const capture = `${sketchDecision}<section class="evidence-inspector-section"><h2><span data-i18n="evidence.capture_details_3be67a">Capture details</span></h2><dl class="evidence-capture-facts">
    <div><dt><span data-i18n="evidence.viewport_91e53b">Viewport</span></dt><dd>${esc(cell.viewport?.name || '—')} · ${esc(cell.viewport?.width)} × ${esc(cell.viewport?.height)}</dd></div>
    <div><dt><span data-i18n="evidence.route_adc747">Route</span></dt><dd>${esc(cell.final_path || cell.requested_path || '—')}</dd></div>
    <div><dt><span data-i18n="evidence.state_a3b50c">State</span></dt><dd>${esc(step.label)}</dd></div>
    <div><dt><span data-i18n="evidence.captured_8a03fa">Captured</span></dt><dd>${(screenshot?.captured_at ? esc(ago(screenshot.captured_at)) : window.DevCoordinatorI18n.markup("evidence.unavailable_ca1844"))}</dd></div>
    <div><dt><span data-i18n="evidence.duration_4fc52a">Duration</span></dt><dd>${cell.duration_ms == null ? '—' : `${(cell.duration_ms / 1000).toFixed(1)}s`}</dd></div>
    <div><dt><span data-i18n="evidence.result_6e7d50">Result</span></dt><dd>${badge(cell.outcome === 'checked' ? 'checked' : cell.outcome, cell.outcome === 'checked' ? 'ok' : 'bad')}</dd></div>

  </dl></section>`;
  const automatic = `<section class="evidence-inspector-section"><h2><span data-i18n="evidence.automated_findings_7ff340">Automated findings</span> <span>${findings.length}</span></h2>${findings.length ? `<ul class="evidence-findings">${findings.map((finding) => `<li class="${esc(finding.severity)}"><button type="button" data-evidence-finding="${esc(finding.rule)}"><i></i><span><strong>${esc(evidenceFindingLabel(finding.rule))}</strong><small>${esc(finding.severity)}</small></span></button></li>`).join('')}</ul><p class="muted evidence-finding-note" id="evidence-finding-note"><span data-i18n="evidence.choose_a_finding_to_return_focus_to_its_capture_92969e">Choose a finding to return focus to its capture.</span></p>` : "<p class=\"muted\"><span data-i18n=\"evidence.no_automatic_visual_findings_for_this_capture_bc3ca1\">No automatic visual findings for this capture.</span></p>"}</section>`;
  const threadList = `<section class="evidence-inspector-section evidence-annotations"><h2><span data-i18n="evidence.annotations_1e4d67">Annotations</span> <span>${threads.length}</span></h2>${threads.length ? threads.map((thread, index) => `<button type="button" class="evidence-thread-summary${selected?.feedback_id === thread.feedback_id ? ' active' : ''}" data-evidence-feedback="${esc(thread.feedback_id)}"><i>${index + 1}</i><span><strong>${(thread.comments[0]?.body ? esc(thread.comments[0]?.body) : window.DevCoordinatorI18n.markup("evidence.visual_feedback_a19903"))}</strong><small>${esc(thread.state)} · ${esc(thread.author)}</small></span></button>`).join('') : "<p class=\"muted\"><span data-i18n=\"evidence.no_feedback_on_this_screenshot_yet_d8a0d7\">No feedback on this screenshot yet.</span></p>"}</section>`;
  if (!selected) {
    return `${capture}${sketchSetComment}${automatic}${threadList}`;
  }
  const comments = selected.comments.map((comment) => `<article class="evidence-comment" data-comment="${esc(comment.comment_id)}"><header><strong>${esc(comment.author)}</strong><small>${window.DevCoordinatorI18n.computedMarkup(() => ago(comment.created_at))}</small></header><p>${esc(comment.body)}</p>${comment.can_edit && !comment.deleted ? `<div class="actions"><button class="btn btn-small" type="button" data-evidence-edit-comment="${esc(comment.comment_id)}"><span data-i18n="evidence.edit_464c4f">Edit</span></button></div>` : ''}</article>`).join('');
  const threadActions = ['sketch', 'sketch-set'].includes(state.evidenceSource) ? '' : `<form id="evidence-feedback-reply"><label class="f"><span data-i18n="evidence.reply_c253f4">Reply</span><textarea name="body" rows="3" maxlength="2000" required></textarea></label><button class="btn" type="submit"><span data-i18n="evidence.reply_c253f4">Reply</span></button></form><div class="evidence-thread-actions"><button class="btn" type="button" data-evidence-state="${selected.state === 'resolved' ? 'open' : 'resolved'}">${(selected.state === 'resolved' ? window.DevCoordinatorI18n.markup("evidence.reopen_a886d1") : window.DevCoordinatorI18n.markup("evidence.resolve_c8f193"))}</button>${selected.can_delete ? "<button class=\"btn btn-danger\" type=\"button\" data-evidence-delete><span data-i18n=\"evidence.delete_annotation_674a79\">Delete annotation</span></button>" : ''}</div>`;
  const thread = `<section class="evidence-inspector-section evidence-thread"><div class="evidence-thread-head"><h2><span data-i18n="evidence.discussion_5eb6cf">Discussion</span></h2><button class="btn btn-small" type="button" data-evidence-feedback-back><span data-i18n="evidence.all_annotations_9decdd">All annotations</span></button></div><div class="evidence-thread-state">${badge(selected.state, selected.state === 'resolved' ? 'ok' : 'warn')}${selected.task_id ? `<a href="#/plan/${esc(state.evidenceData.repository_id)}" data-evidence-open-task="${esc(selected.task_id)}"><span data-i18n="evidence.open_plan_task_4019fe">Open Plan task →</span></a>` : ''}</div>${comments}${threadActions}</section>`;
  return `${capture}${sketchSetComment}${automatic}${threadList}${thread}`;

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
  const expanded = (panel) => fullscreen ? fullscreenPanels[panel] : preferences[panel] ?? (panel === 'journey' || panel === 'details' && state.evidenceSource === 'sketch-set' && page.clientWidth >= 760 || page.clientWidth >= 1050);
  const paint = () => {
    if (!page.isConnected) return;
    const narrow = page.clientWidth < 1050;
    const mobileDetails = narrow && window.matchMedia('(max-width: 1180px)').matches;
    page.classList.toggle('evidence-layout-narrow', narrow);
    page.classList.toggle('evidence-is-fullscreen', fullscreen);
    page.style.height = `${fullscreen ? innerHeight : Math.max(520, innerHeight - Math.max(0, page.getBoundingClientRect().top) - 1)}px`;
    for (const [panel, selector, label] of [['journey', '#evidence-journey', 'journey panel'], ['details', '#evidence-inspector', 'capture details']]) {
      const visible = expanded(panel);
      // Keep the narrow details bar mounted while its body is closed so the
      // user still has a control to reopen postmortem review.
      $(selector, page).hidden = !visible && !(panel === 'details' && mobileDetails);
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
    if (focus) (visible && panel === 'details'
      ? (page.clientWidth < 1050 && window.matchMedia('(max-width: 1180px)').matches ? $('.evidence-mobile-inspector-toggle', page) : $('.evidence-panel-close', page))
      : $(`[data-evidence-panel="${panel}"]:not(.evidence-panel-close)`, page))?.focus({ preventScroll: true });
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
      window.DevCoordinatorI18n.bind(submit, () => (state.evidencePendingLabel ? window.DevCoordinatorI18n.t("common.add_or_cancel_the_text_label_before_saving_37b28b") : (!state.evidenceDraftMarks.length ? window.DevCoordinatorI18n.t("common.add_a_mark_to_attach_this_comment_1e382b") : '')), "title");
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
    editor.innerHTML = "<input class=\"evidence-text-entry\" aria-label=\"Annotation label\" placeholder=\"Annotation label\" maxlength=\"120\" required data-i18n-attrs='{\"aria-label\":\"evidence.annotation_label_2c07cd\",\"placeholder\":\"evidence.annotation_label_2c07cd\"}'><div class=\"actions\"><button class=\"btn btn-small\" type=\"button\"><span data-i18n=\"evidence.cancel_19766e\">Cancel</span></button><button class=\"btn btn-primary btn-small\" type=\"submit\"><span data-i18n=\"evidence.add_label_e3488b\">Add label</span></button></div>";
    const input = $('input', editor);
    input.value = state.evidencePendingLabel?.text || '';
    const cancel = () => { state.evidencePendingLabel = null; editor.remove(); updateEvidenceToolbar(); $('#evidence-canvas', main)?.focus({ preventScroll: true }); };
    $('button[type=button]', editor).addEventListener('click', cancel);
    editor.addEventListener('keydown', (event) => { if (event.key === 'Escape') { event.preventDefault(); cancel(); } });
    editor.addEventListener('submit', (event) => {
      event.preventDefault(); const text = input.value.trim();
      if (!text) { input.setCustomValidity(window.DevCoordinatorI18n.t("common.enter_a_label_6eeb5d")); input.reportValidity(); return; }
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
  return `<div class="evidence-toolbar" role="toolbar" aria-label="Screenshot annotation tools" data-i18n-attrs='{"aria-label":"evidence.screenshot_annotation_tools_83c8b4"}'>
    <div class="evidence-tool-group">${EVIDENCE_TOOLS.map(([tool, icon, label]) => `<button type="button" class="evidence-tool${tool === state.evidenceTool ? ' active' : ''}" data-evidence-tool="${tool}" aria-label="${label}" title="${label}" aria-pressed="${tool === state.evidenceTool}">${planIcon(icon)}<span>${label}</span></button>`).join('')}</div>
    <label class="evidence-color" title="Annotation colour" data-i18n-attrs='{"title":"evidence.annotation_colour_119970"}'>${planIcon('palette')}<span class="sr-only"><span data-i18n="evidence.annotation_colour_119970">Annotation colour</span></span><select id="evidence-color" aria-label="Annotation colour" data-i18n-attrs='{"aria-label":"evidence.annotation_colour_119970"}'>${EVIDENCE_COLORS.map(([color, label]) => `<option value="${color}"${color === state.evidenceColor ? ' selected' : ''}>${label}</option>`).join('')}</select><i style="--mark-color:${state.evidenceColor}"></i></label>
    <div class="evidence-tool-group evidence-history"><button type="button" class="evidence-tool" data-evidence-compose aria-label="Edit comment" title="Edit comment" disabled data-i18n-attrs='{"aria-label":"evidence.edit_comment_4f346f","title":"evidence.edit_comment_4f346f"}'>${planIcon('message-plus')}<span><span data-i18n="evidence.edit_comment_4f346f">Edit comment</span></span></button><button type="button" class="evidence-tool" data-evidence-undo aria-label="Undo" title="Undo" disabled data-i18n-attrs='{"aria-label":"evidence.undo_a8283a","title":"evidence.undo_a8283a"}'>${planIcon('arrow-back-up')}</button><button type="button" class="evidence-tool" data-evidence-redo aria-label="Redo" title="Redo" disabled data-i18n-attrs='{"aria-label":"evidence.redo_742739","title":"evidence.redo_742739"}'>${planIcon('arrow-forward-up')}</button></div>
    <div class="evidence-tool-group evidence-zoom"><button type="button" class="evidence-tool" data-evidence-zoom-out aria-label="Zoom out" title="Zoom out" data-i18n-attrs='{"aria-label":"evidence.zoom_out_bc7b63","title":"evidence.zoom_out_bc7b63"}'>${planIcon('zoom-out')}</button><output id="evidence-zoom-value">100%</output><button type="button" class="evidence-tool" data-evidence-zoom-in aria-label="Zoom in" title="Zoom in" data-i18n-attrs='{"aria-label":"evidence.zoom_in_0e47f0","title":"evidence.zoom_in_0e47f0"}'>${planIcon('zoom-in')}</button><button type="button" class="evidence-tool" data-evidence-fit aria-label="Fit screenshot" title="Fit screenshot" data-i18n-attrs='{"aria-label":"evidence.fit_screenshot_39e530","title":"evidence.fit_screenshot_39e530"}'>${planIcon('focus-centered')}</button><button type="button" class="evidence-tool" data-evidence-clear aria-label="Clear unsaved marks" title="Clear unsaved marks" disabled data-i18n-attrs='{"aria-label":"evidence.clear_unsaved_marks_f40d39","title":"evidence.clear_unsaved_marks_f40d39"}'>${planIcon('trash')}</button></div>
    <div class="evidence-tool-group evidence-layout-controls"><button type="button" class="evidence-tool" data-evidence-panel="journey" aria-controls="evidence-journey" aria-label="Hide journey panel" title="Hide journey panel" data-i18n-attrs='{"aria-label":"evidence.hide_journey_panel_7f8d6f","title":"evidence.hide_journey_panel_7f8d6f"}'>${planIcon('layout-sidebar-left-collapse')}<span><span data-i18n="evidence.journey_fe50ed">Journey</span></span></button><button type="button" class="evidence-tool" data-evidence-panel="details" aria-controls="evidence-inspector" aria-label="Hide capture details" title="Hide capture details" data-i18n-attrs='{"aria-label":"evidence.hide_capture_details_8ef22e","title":"evidence.hide_capture_details_8ef22e"}'>${planIcon('layout-sidebar-left-collapse')}<span><span data-i18n="evidence.details_45989d">Details</span></span></button><button type="button" class="evidence-tool" data-evidence-fullscreen aria-label="Full screen" title="Full screen" aria-pressed="false" data-i18n-attrs='{"aria-label":"evidence.full_screen_674fe2","title":"evidence.full_screen_674fe2"}'><svg class="evidence-fullscreen-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 9V4h5m6 0h5v5m0 6v5h-5m-6 0H4v-5"/></svg><span class="evidence-fullscreen-label"><span data-i18n="evidence.full_screen_674fe2">Full screen</span></span></button></div>
  </div>`;
}

function renderEvidenceCurrent(step, cell) {
  const current = state.evidenceSteps.indexOf(step);
  const viewportAvailable = cell.screenshots?.viewport?.status === 'available';
  const fullAvailable = cell.screenshots?.full_page?.status === 'available';
  const route = state.evidenceSource === 'sketch-set' ? '' : `<span>${cell.final_path || cell.requested_path ? esc(cell.final_path || cell.requested_path) : window.DevCoordinatorI18n.markup("evidence.route_unavailable_21a43f")}</span>`;
  return `<div class="evidence-current-copy"><strong>${window.DevCoordinatorI18n.markup("evidence.step_value1_of_value2_196d3d", {value1: current + 1, value2: state.evidenceSteps.length})}</strong><h2>${esc(step.label)}</h2>${route}</div>
    <div class="evidence-current-actions"><div class="seg evidence-capture-kind" role="tablist" aria-label="Screenshot kind" data-i18n-attrs='{"aria-label":"evidence.screenshot_kind_19a142"}'><button type="button" data-evidence-kind="viewport" class="${state.evidenceScreenshotKind === 'viewport' ? 'active' : ''}"${viewportAvailable ? '' : ' disabled'}><span data-i18n="evidence.viewport_91e53b">Viewport</span></button><button type="button" data-evidence-kind="full_page" class="${state.evidenceScreenshotKind === 'full_page' ? 'active' : ''}"${fullAvailable ? '' : ' disabled'}><span data-i18n="evidence.full_page_9d21b6">Full page</span></button></div><button class="btn btn-small" type="button" data-evidence-prev aria-label="Previous journey step"${current <= 0 ? ' disabled' : ''} data-i18n-attrs='{"aria-label":"evidence.previous_journey_step_7a544d"}'>${planIcon('chevron-left')}</button><button class="btn btn-small" type="button" data-evidence-next aria-label="Next journey step"${current >= state.evidenceSteps.length - 1 ? ' disabled' : ''} data-i18n-attrs='{"aria-label":"evidence.next_journey_step_35037a"}'>${planIcon('chevron-right')}</button></div>`;

}

function evidenceWorkspace(run, data) {
  const openFeedback = (data.feedback || []).filter((item) => item.state === 'open').length;
  const sketchEvidence = ['sketch', 'sketch-set'].includes(state.evidenceSource);
  const home = sketchEvidence ? '#/sketches' : '#/tests';
  const label = window.DevCoordinatorI18n.t(sketchEvidence ? "sketches.sketches_a56d78" : "common.destination_tests");
  const breadcrumb = `<a class="destination-link" href="${home}">${label}</a><span>/</span>${workspace.active ? '' : `<strong>${esc(run.display_name)}</strong><span>/</span>`}`;
  const close = state.evidenceSource === 'sketch-set' ? `<a class="btn btn-small sketch-set-close" href="#/sketches/${esc(state.sketchRepositoryId)}">${window.DevCoordinatorI18n.markup("evidence.closeReview")}</a>` : '';
  const setMeta = state.evidenceSource === 'sketch-set' ? window.DevCoordinatorI18n.markup("evidence.selectionSummary", {selected: state.sketchSetGroup?.sketches.filter((item) => item.decision === 'keep').length || 0, total: state.sketchSetGroup?.sketches.length || 0}) : `<span class="mono">${esc(run.run_id)}</span>${run.isEarlierEvidence ? badge('Earlier visual run') : `${badge(run.status)}<span class="evidence-run-meta">${window.DevCoordinatorI18n.computedMarkup(() => testTierLabel(run.requested_tier))}</span><span class="evidence-run-meta">${(run.readiness_eligible ? window.DevCoordinatorI18n.markup("evidence.release_proof_17fa7c") : window.DevCoordinatorI18n.markup("evidence.diagnostic_only_3425ba"))}</span>`}<span class="evidence-run-meta">${window.DevCoordinatorI18n.computedMarkup(() => ago(run.started_at))}</span>`;
  return `<section class="evidence-page${state.evidenceSource === 'sketch-set' ? ' sketch-set-evidence-page' : ''}" data-ui-region="test-evidence-primary">
    <header class="evidence-page-head"><div><h1 class="evidence-breadcrumb">${breadcrumb}<strong>${(run.test ? esc(run.test) : window.DevCoordinatorI18n.markup("evidence.test_run_fe9e2f"))}</strong></h1><div class="evidence-run-line">${setMeta}</div></div><div class="evidence-review-state"><span><span data-i18n="evidence.review_status_a410d7">Review status</span></span><span class="badge ${openFeedback ? 'warn' : 'ok'}">${openFeedback ? window.DevCoordinatorI18n.markup("evidence.changesRequested", {count:openFeedback}) : window.DevCoordinatorI18n.markup("evidence.noChangesRequested")}</span>${close}</div></header>
    <div class="evidence-board">
      <aside id="evidence-journey" class="evidence-rail" aria-label="Journey steps" data-i18n-attrs='{"aria-label":"evidence.journey_steps_a9b3d0"}'><div class="evidence-rail-head"><h2><span data-i18n="evidence.journey_fe50ed">Journey</span></h2><span>${window.DevCoordinatorI18n.markup("evidence.value7_steps_531b1f", {value7: state.evidenceSteps.length})}</span></div><div id="evidence-step-list">${renderEvidenceRail()}</div></aside>
      <section class="evidence-workspace" data-ui-region="test-evidence-workspace"><header id="evidence-current" class="evidence-current"></header>${evidenceToolbar()}<div id="evidence-scroll" class="evidence-scroll"><div id="evidence-media" class="evidence-media"><img id="evidence-image" alt="Selected user journey screenshot" data-i18n-attrs='{"alt":"evidence.selectedJourneyScreenshot"}' hidden><canvas id="evidence-canvas" tabindex="0" aria-label="Screenshot annotation canvas" data-i18n-attrs='{"aria-label":"evidence.screenshot_annotation_canvas_07eeb5"}'></canvas></div><div id="evidence-image-state" class="evidence-image-state"><span data-i18n="evidence.loading_screenshot_e0fd7b">Loading screenshot…</span></div></div><section class="evidence-compare" aria-labelledby="evidence-compare-title"><div><h2 id="evidence-compare-title">${state.evidenceSource === 'sketch-set' ? window.DevCoordinatorI18n.markup("evidence.allSketchOptions") : window.DevCoordinatorI18n.markup("evidence.viewport_comparison_e876b4")}</h2><span>${state.evidenceSource === 'sketch-set' ? window.DevCoordinatorI18n.markup("evidence.selectOptionToAnnotate") : window.DevCoordinatorI18n.markup("evidence.same_journey_moment_4520e6")}</span></div><div id="evidence-variants" class="evidence-variants"></div></section></section>
      <aside id="evidence-inspector" class="evidence-inspector" aria-label="Capture details and feedback" data-i18n-attrs='{"aria-label":"evidence.capture_details_and_feedback_40a8be"}'></aside>

    </div>
  </section>`;
}

function replaceEvidenceFeedback(feedback) {
  const rows = state.evidenceData.feedback || [];
  const index = rows.findIndex((item) => item.feedback_id === feedback.feedback_id);
  if (index >= 0) rows[index] = feedback; else rows.push(feedback);
}

async function evidenceMutation(button, command, args, onError = error => toast(() => error.message, 'bad')) {
  const runId = state.evidenceRun.run_id;
  button.disabled = true;
  try {
    const imageId = currentEvidenceSelection().screenshot?.image_id || state.evidenceRun.run_id;
    const result = ['sketch', 'sketch-set'].includes(state.evidenceSource)
      ? await api('design.sketch.annotation.create', { repository_id: state.sketchRepositoryId, sketch_id: imageId, body: args.body, marks: args.marks }, false)
      : await api(command, { path: state.evidenceRun.worktree_path, run_id: state.evidenceRun.run_id, ...args }, false);
    if (['sketch', 'sketch-set'].includes(state.evidenceSource) && result.annotation) {
      const annotation = result.annotation;
      if (state.evidenceSource === 'sketch-set') {
        const detail = state.sketchSetDetails.get(imageId);
        if (detail) state.sketchSetDetails.set(imageId, { ...detail, annotations: [...(detail.annotations || []), annotation] });
      }
      replaceEvidenceFeedback({ feedback_id: annotation.annotation_id, task_id: '', task_status: 'planned', state: annotation.state, image_id: imageId, marks: annotation.marks, author: annotation.author, created_at: annotation.created_at, updated_at: annotation.updated_at, comments: [{ comment_id: annotation.annotation_id, body: annotation.body, author: annotation.author, created_at: annotation.created_at, updated_at: annotation.updated_at, can_edit: false, deleted: false }], can_delete: annotation.can_delete });
    }
    if (result.feedback && state.evidenceRun?.run_id === runId) replaceEvidenceFeedback(result.feedback);
    return result;
  } catch (error) {
    onError(error); return null;
  } finally { button.disabled = false; }
}

function bindEvidenceInspector() {
  $('#sketch-set-comment-form', main)?.addEventListener('submit', async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const body = String(new FormData(form).get('body') || '').trim();
    const scope = String(new FormData(form).get('scope') || 'active');
    if (body.length < 3 || state.evidenceSource !== 'sketch-set') return;
    const activeId = state.evidenceRun.run_id;
    const group = state.sketchSetGroup;
    const targets = scope === 'selected'
      ? group.sketches.filter((item) => item.decision === 'keep')
      : group.sketches.filter((item) => item.sketch_id === activeId);
    const button = form.querySelector('button[type="submit"]');
    if (!targets.length) return;
    button.disabled = true;
    try {
      for (const target of targets) {
        const result = await api('design.sketch.annotation.create', { repository_id: state.sketchRepositoryId, sketch_id: target.sketch_id, body, marks: [] }, false);
        const annotation = result.annotation;
        if (annotation) replaceEvidenceFeedback({ feedback_id: annotation.annotation_id, task_id: '', task_status: 'planned', state: annotation.state, image_id: target.sketch_id, marks: annotation.marks, author: annotation.author, created_at: annotation.created_at, updated_at: annotation.updated_at, comments: [{ comment_id: annotation.annotation_id, body: annotation.body, author: annotation.author, created_at: annotation.created_at, updated_at: annotation.updated_at, can_edit: false, deleted: false }], can_delete: annotation.can_delete });
      }
      form.reset(); refreshEvidenceInspector(); toast(() => window.DevCoordinatorI18n.t("evidence.commentAdded", {count:targets.length}), 'ok');
    } catch (error) { toast(() => window.DevCoordinatorI18n.t("evidence.commentFailed", {message:error.message}), 'bad'); }
    finally { button.disabled = false; }
  });
  $('[data-sketch-record]', main)?.addEventListener('click', async (event) => {
    const button = event.currentTarget; button.disabled = true;
    try {
      const chunks = []; let offset = 0; let total = null;
      const recordSketchId = currentEvidenceSelection().screenshot?.image_id || state.evidenceRun.run_id;
      do { const part = await api('design.sketch.record', { repository_id: state.sketchRepositoryId, sketch_id: recordSketchId, offset, max_bytes: 184320 }); const bytes = Uint8Array.from(atob(part.base64 || ''), (c) => c.charCodeAt(0)); chunks.push(bytes); total = part.total_bytes; offset += bytes.length; if (part.next_offset == null) break; } while (offset < total);
      const text = new TextDecoder().decode(concatBytes(chunks));
      const details = document.createElement('details'); details.className = 'sketch-record'; details.open = true; details.innerHTML = `<summary><span data-i18n="evidence.generation_record_5e6033">Generation record</span></summary><pre></pre>`; details.querySelector('pre').textContent = text;
      button.replaceWith(details);
    } catch (error) { toast(() => window.DevCoordinatorI18n.t("common.generation_record_unavailable_value1_b601a8", {value1: error.message}), 'bad'); } finally { button.disabled = false; }
  });
  main.querySelectorAll('[data-sketch-decision]').forEach((button) => button.addEventListener('click', async () => {
    if (!['sketch', 'sketch-set'].includes(state.evidenceSource) || !state.sketchDetail) return;
    const rationale = window.prompt(window.DevCoordinatorI18n.t("evidence.sketchChangeWhy"), window.DevCoordinatorI18n.t("evidence.reviewedInConsole"))?.trim();
    if (!rationale) return;
    button.disabled = true;
    try {
      const result = await api('design.sketch.decision', { repository_id: state.sketchRepositoryId, sketch_id: state.evidenceRun.run_id, expected_revision: state.sketchDetail.sketch.decision_revision, decision: button.dataset.sketchDecision, rationale }, false);
      state.sketchDetail = { ...state.sketchDetail, sketch: result.sketch, history: [...(state.sketchDetail.history || []), result.event] };
      if (state.evidenceSource === 'sketch-set') state.sketchSetDetails.set(result.sketch.sketch_id, state.sketchDetail);
      const summary = state.sketchGalleryData?.find((item) => item.sketch_id === result.sketch.sketch_id);
      if (summary) Object.assign(summary, { decision: result.sketch.decision, decision_revision: result.sketch.decision_revision });
      if (state.evidenceSource === 'sketch-set') refreshEvidenceSelection(); else refreshEvidenceInspector();
      toast(() => window.DevCoordinatorI18n.t("common.sketch_decision_saved_e11ab3"), 'ok');
    } catch (error) { toast(() => window.DevCoordinatorI18n.t("common.decision_failed_value1_c85449", {value1: error.message}), 'bad'); } finally { button.disabled = false; }

  }));
  main.querySelectorAll('[data-evidence-feedback]').forEach((button) => button.addEventListener('click', () => {
    state.evidenceSelectedFeedbackId = button.dataset.evidenceFeedback;
    evidenceLayoutSession?.showDetails();
    $('.evidence-page', main)?.classList.add('inspector-open');
    refreshEvidenceInspector(); redrawEvidenceCanvas();
  }));
  main.querySelectorAll('[data-evidence-finding]').forEach((button) => button.addEventListener('click', () => {
    main.querySelectorAll('[data-evidence-finding]').forEach((item) => item.classList.toggle('active', item === button));
    const note = $('#evidence-finding-note', main);
    if (note) window.DevCoordinatorI18n.text(note, "common.this_finding_applies_to_the_selected_capture_pri_694be9");
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
    article.innerHTML = `<form class="evidence-comment-edit"><label class="f"><span data-i18n="evidence.edit_comment_4f346f">Edit comment</span><textarea name="body" rows="4" maxlength="2000">${esc(comment.body)}</textarea></label><div class="actions"><button class="btn btn-primary btn-small" type="submit"><span data-i18n="evidence.save_1509f5">Save</span></button><button class="btn btn-small" type="button" data-edit-cancel><span data-i18n="evidence.cancel_19766e">Cancel</span></button></div></form>`;
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
      redrawEvidenceCanvas(); updateEvidencePageStatus(); toast(() => window.DevCoordinatorI18n.t("common.annotation_deleted_792946"), 'ok');
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
  inspector.innerHTML = `<button type="button" class="evidence-mobile-inspector-toggle" data-evidence-panel="details" aria-controls="evidence-inspector-body" aria-expanded="false"><span><span data-i18n="evidence.capture_details_and_feedback_40a8be">Capture details and feedback</span></span>${planIcon('chevron-up')}</button><button type="button" class="evidence-tool evidence-panel-close" data-evidence-panel="details" aria-label="Hide capture details" title="Hide capture details" data-i18n-attrs='{"aria-label":"evidence.hide_capture_details_8ef22e","title":"evidence.hide_capture_details_8ef22e"}'>${planIcon('x')}</button><div id="evidence-inspector-body" class="evidence-inspector-body">${renderEvidenceInspector(state.evidenceRun, step, cell, screenshot)}</div>`;
  $('h2', inspector)?.setAttribute('id', 'evidence-details-heading');
  bindEvidenceInspector();
}

function updateEvidencePageStatus() {
  const target = $('.evidence-review-state', main); if (!target) return;
  const open = (state.evidenceData.feedback || []).filter((item) => item.state === 'open').length;
  target.innerHTML = `<span><span data-i18n="evidence.review_status_a410d7">Review status</span></span>${open ? badge(`${open} changes requested`, 'warn') : badge('No changes requested', 'ok')}`;
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
  if (!screenshot) { window.DevCoordinatorI18n.text(status, "common.this_journey_step_has_no_retained_screenshot_18eab3"); status.hidden = false; return; }
  window.DevCoordinatorI18n.text(status, "common.loading_screenshot_e0fd7b"); status.hidden = false; media.style.width = `${state.evidenceZoom * 100}%`;
  const requested = screenshot.image_id;
  try {
    const url = await evidenceImageUrl(run, screenshot);
    if (currentEvidenceSelection().screenshot?.image_id !== requested || !image?.isConnected) return;
    image.src = url; image.hidden = false; await image.decode().catch(() => {});
    status.hidden = true; setupEvidenceCanvas(requested);
    if (state.evidenceSource === 'sketch-set') $('#evidence-canvas', main)?.focus({ preventScroll: true });
    if (state.evidencePendingLabel) evidenceTextEntry(state.evidencePendingLabel.point, false);
  } catch (error) {
    window.DevCoordinatorI18n.bind(status, () => (error.message || window.DevCoordinatorI18n.t("common.screenshot_unavailable_05e5d1"))); status.hidden = false;
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
  if (state.evidenceSource === 'sketch-set' && screenshot?.image_id) {
    state.evidenceRun.run_id = screenshot.image_id;
    state.evidenceRunId = screenshot.image_id;
    state.sketchDetail = state.sketchSetDetails.get(screenshot.image_id) || state.sketchDetail;
  }
  activateEvidenceDraft(screenshot?.image_id);
  if (screenshot?.image_id) {
    const query = new URLSearchParams(location.hash.split('?')[1] || '');
    query.set('image', screenshot.image_id);
    if (state.evidenceRun.worktree_id) query.set('worktree', state.evidenceRun.worktree_id);
    const prefix = ['sketch', 'sketch-set'].includes(state.evidenceSource) ? `#/sketches/${state.sketchRepositoryId}` : `#/tests/${state.evidenceRunId}`;
    if (state.evidenceSource === 'sketch') query.set('sketch', state.evidenceRunId);
    if (state.evidenceSource === 'sketch-set') {
      query.set('set', state.sketchSetGroup?.key || '');
      query.set('sketch', state.evidenceRun.run_id);
    }
    window.history.replaceState(null, '', `${prefix}?${query}`);
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
      main.innerHTML = `${pageHeading('Tests', '#/tests', run.display_name)}${stateBlock('empty', () => window.DevCoordinatorI18n.t("common.this_screenshot_is_not_available_for_this_run_4a0278"))}`;
      return;
    }
  }
  if (!state.evidenceSteps.length) {
    main.innerHTML = `${pageHeading('Tests', '#/tests', run.display_name)}${stateBlock('empty', () => (data.issues?.length ? window.DevCoordinatorI18n.t("common.visual_evidence_was_invalid_and_could_not_be_ope_8db452") : window.DevCoordinatorI18n.t("common.this_run_did_not_publish_visual_journey_evidence_8370da")))}`; return;
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

function sketchFeedback(detail) {
  return (detail.annotations || []).map((annotation) => ({
    feedback_id: annotation.annotation_id, task_id: '', task_status: 'planned', state: annotation.state,
    image_id: detail.sketch.sketch_id, marks: annotation.marks, author: annotation.author,
    created_at: annotation.created_at, updated_at: annotation.updated_at, can_delete: annotation.can_delete,
    comments: [{ comment_id: annotation.annotation_id, body: annotation.body, author: annotation.author,
      created_at: annotation.created_at, updated_at: annotation.updated_at, deleted: false, can_edit: false }], comments_truncated: false,
  }));
}
function concatBytes(chunks) { const total = chunks.reduce((n, chunk) => n + chunk.length, 0); const output = new Uint8Array(total); let offset = 0; for (const chunk of chunks) { output.set(chunk, offset); offset += chunk.length; } return output; }

async function sketchImageUrl(repositoryId, sketch) {
  const sketchId = sketch.image_id || sketch.sketch_id;
  if (!sketchId) throw new ApiError('design_sketch_not_found', 'Sketch image unavailable');
  const cacheKey = `sketch:${repositoryId}:${sketchId}`;
  if (state.evidenceImageUrls.has(cacheKey)) return state.evidenceImageUrls.get(cacheKey);
  if (state.evidenceImagePromises.has(cacheKey)) return state.evidenceImagePromises.get(cacheKey);
  const promise = (async () => {
    const chunks = [];
    let offset = 0;
    let total = null;
    let mime = sketch.mime || 'image/png';
    for (let guard = 0; guard < 512; guard += 1) {
      const part = await api('design.sketch.image', {
        repository_id: repositoryId, sketch_id: sketchId, offset, max_bytes: 184320,
      });
      if (Number(part.offset) !== offset) throw new ApiError('design_sketch_invalid_chunk', 'Sketch image returned an unexpected chunk offset');
      if (total == null) total = Number(part.total_bytes);
      if (Number(part.total_bytes) !== total) throw new ApiError('design_sketch_invalid_chunk', 'Sketch image changed while it was being read');
      mime = part.mime || mime;
      const binary = atob(part.base64 || '');
      const bytes = new Uint8Array(binary.length);
      for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
      if (!bytes.length && part.next_offset != null) throw new ApiError('design_sketch_invalid_chunk', 'Sketch image returned an empty intermediate chunk');
      chunks.push(bytes);
      const nextOffset = part.next_offset == null ? null : Number(part.next_offset);
      if (nextOffset == null) {
        offset += bytes.length;
        if (offset !== total) throw new ApiError('design_sketch_invalid_chunk', 'Sketch image ended before all bytes were read');
        break;
      }
      if (nextOffset <= offset || nextOffset > total || nextOffset !== offset + bytes.length) {
        throw new ApiError('design_sketch_invalid_chunk', 'Sketch image returned a non-contiguous chunk');
      }
      offset = nextOffset;
      if (guard === 511) throw new ApiError('design_sketch_invalid_chunk', 'Sketch image has too many chunks');
    }
    const url = URL.createObjectURL(new Blob(chunks, { type: mime }));
    state.evidenceImageUrls.set(cacheKey, url);
    state.evidenceImagePromises.delete(cacheKey);
    return url;
  })().catch((error) => {
    state.evidenceImagePromises.delete(cacheKey);
    throw error;
  });
  state.evidenceImagePromises.set(cacheKey, promise);
  return promise;
}

const sketchImageQueue = [];
let sketchImageLoads = 0;
const SKETCH_IMAGE_CONCURRENCY = 4;
function pumpSketchImageQueue() {
  while (sketchImageLoads < SKETCH_IMAGE_CONCURRENCY && sketchImageQueue.length) {
    const job = sketchImageQueue.shift();
    sketchImageLoads += 1;
    Promise.resolve().then(job.run).then(job.resolve, job.reject).finally(() => {
      sketchImageLoads -= 1;
      pumpSketchImageQueue();
    });
  }
}
function enqueueSketchImage(run) {
  return new Promise((resolve, reject) => {
    sketchImageQueue.push({ run, resolve, reject });
    pumpSketchImageQueue();
  });
}

function sketchEvidence(detail) {
  const s = detail.sketch;
  return {
    repository_id: s.repository_id, feedback: sketchFeedback(detail), issues: [],
    bundles: [{ formal_run_id: `sketch-${s.sketch_id}`, generated_at: s.created_at, browser: s.source_skill,
      check: 'sketch', phase: 'design', case: s.sketch_set, coverage: { checked_pages: 1, planned_pages: 1, failed: false, readiness_eligible: false },
      cells: [{ cell_id: s.sketch_id, review_cell_key: null, plan_index: 0, target_name: s.title,
        primary_journey: s.sketch_set, state_name: s.title, requested_path: null, final_path: null,
        viewport: { name: `${s.width} × ${s.height}`, width: s.width, height: s.height, device: null },
        started_at: s.created_at, ended_at: s.created_at, duration_ms: null, outcome: 'checked', http_status: null,
        source_binding_status: 'sketch', review: null, actions: [], findings: [],
        screenshots: { viewport: { status: 'available', kind: 'viewport', image_id: s.sketch_id, mime: s.mime, size: s.byte_size, sha256: s.sha256, width: s.width, height: s.height, captured_at: s.created_at }, full_page: null }, formal_run_id: `sketch-${s.sketch_id}` }],
    }], image_count: 1, status: 'available', run_id: s.sketch_id, worktree_id: '',
  };
}

function sketchSetGroups(sketches) {
  const groups = new Map();
  for (const sketch of sketches || []) {
    const key = sketch.sketch_set || 'Unlabeled sketch set';
    if (!groups.has(key)) groups.set(key, { key, sketches: [] });
    groups.get(key).sketches.push(sketch);
  }
  return [...groups.values()];
}

function sketchGroupSummary(group) {
  const selected = group.sketches.filter((sketch) => sketch.decision === 'keep');
  const rejected = group.sketches.filter((sketch) => sketch.decision === 'reject');
  return { selected, rejected, pending: group.sketches.length - selected.length - rejected.length };
}

function sketchRoute(repositoryId, setName = null, sketchId = null) {
  const query = new URLSearchParams();
  if (setName) query.set('set', setName);
  if (sketchId) query.set('sketch', sketchId);
  const suffix = query.toString() ? `?${query.toString()}` : '';
  return `#/sketches/${encodeURIComponent(repositoryId)}${suffix}`;
}

function sketchGallerySketch(sketchId) {
  return (state.sketchGalleryData || []).find((sketch) => sketch.sketch_id === sketchId) || null;
}

function renderSketchCard(repositoryId, group, sketch) {
  const selected = sketch.decision === 'keep';
  const toggleLabel = selected ? 'Selected' : sketch.decision === 'reject' ? 'Select again' : 'Select';
  return `<article class="sketch-card${selected ? ' is-selected' : ''}">
    <div class="sketch-card-image-wrap">
      <a class="sketch-card-image" href="${esc(sketchRoute(repositoryId, null, sketch.sketch_id))}"><img loading="lazy" decoding="async" alt="${esc(sketch.title)}" data-sketch-thumb="${esc(sketch.sketch_id)}"><span class="muted">${esc(sketch.width)} × ${esc(sketch.height)}</span></a>
      <button type="button" class="sketch-card-select${selected ? ' active' : ''}" data-sketch-toggle="${esc(sketch.sketch_id)}" aria-pressed="${selected}"><i aria-hidden="true">${selected ? '✓' : ''}</i><span>${toggleLabel}</span></button>
    </div>
    <div class="sketch-card-body"><h3>${esc(sketch.title)}</h3><p class="muted">${esc(sketch.source_skill)}</p><p>${badge(sketch.decision, sketch.decision === 'keep' ? 'ok' : sketch.decision === 'reject' ? 'bad' : '')}</p><div class="actions"><button class="btn btn-small" type="button" data-sketch-review-set="${esc(group.key)}" data-sketch-review-sketch="${esc(sketch.sketch_id)}"><span data-i18n="sketches.review_set_ca242c">Review set</span></button><a class="btn btn-small" href="${esc(sketchRoute(repositoryId, null, sketch.sketch_id))}"><span data-i18n="sketches.open_sketch_3ccc5c">Open sketch</span></a></div></div>
  </article>`;
}

function renderSketchDrawer(repositoryId, group) {
  if (!group) return '';
  const current = group.sketches.find((sketch) => sketch.sketch_id === state.sketchDrawerSketchId) || group.sketches[0];
  const summary = sketchGroupSummary(group);
  const detail = state.sketchDrawerDetail?.sketch?.sketch_id === current?.sketch_id ? state.sketchDrawerDetail : null;
  const annotations = detail?.annotations || [];
  const comments = annotations.length ? annotations.map((annotation) => `<article class="sketch-drawer-comment"><header><strong>${(annotation.author ? esc(annotation.author) : window.DevCoordinatorI18n.markup("sketches.reviewer_d29f46"))}</strong><small>${window.DevCoordinatorI18n.computedMarkup(() => ago(annotation.created_at))}</small></header><p>${esc(annotation.body)}</p></article>`).join('') : "<p class=\"muted\"><span data-i18n=\"sketches.no_comments_on_this_option_yet_801c78\">No comments on this option yet.</span></p>";
  const commentTargets = summary.selected.length || current?.decision === 'keep' ? summary.selected.length || 1 : 1;
  const commentLabel = commentTargets > 1 ? `Comment on ${commentTargets} selected options` : 'Comment on this option';
  return `<div class="sketch-set-drawer-backdrop" data-sketch-drawer-backdrop data-ui-contextual-overlay="Sketch set comparison drawer" role="dialog" aria-modal="true" aria-labelledby="sketch-drawer-title" tabindex="-1">
    <section class="sketch-set-drawer" data-ui-allow-overlap="Sketch set comparison drawer is intentionally positioned above the gallery surface">
      <header class="sketch-set-drawer-head"><div><p class="eyebrow"><span data-i18n="sketches.sketch_set_review_1e4713">Sketch set review</span></p><h2 id="sketch-drawer-title">${esc(group.key)}</h2><p class="muted">${window.DevCoordinatorI18n.markup("sketches.value2_selected_value3_options_b64a24", {value2: summary.selected.length, value3: group.sketches.length})}</p></div><button type="button" class="btn btn-small" data-sketch-drawer-close aria-label="Close set review" data-i18n-attrs='{"aria-label":"sketches.close_set_review_2680d1"}'><span data-i18n="sketches.close_7d9eb7">Close</span></button></header>
      <div class="sketch-drawer-mode" role="group" aria-label="Selection mode" data-i18n-attrs='{"aria-label":"sketches.selection_mode_1935ae"}'><span><span data-i18n="sketches.selection_mode_1935ae">Selection mode</span></span><button type="button" class="seg${state.sketchSelectionMode === 'single' ? ' active' : ''}" data-sketch-mode="single" aria-pressed="${state.sketchSelectionMode === 'single'}"><span data-i18n="sketches.single_choice_45a76a">Single choice</span></button><button type="button" class="seg${state.sketchSelectionMode === 'multiple' ? ' active' : ''}" data-sketch-mode="multiple" aria-pressed="${state.sketchSelectionMode === 'multiple'}"><span data-i18n="sketches.choose_multiple_b76dc5">Choose multiple</span></button></div>
      <div class="sketch-drawer-media"><div class="sketch-drawer-image"><img loading="eager" decoding="async" alt="${esc(current?.title || 'Selected sketch')}" data-sketch-drawer-image="${esc(current?.sketch_id || '')}"><span class="muted">${esc(current?.width || '—')} × ${esc(current?.height || '—')}</span></div><div class="sketch-drawer-variants" aria-label="Sketch set options" data-i18n-attrs='{"aria-label":"sketches.sketch_set_options_62656f"}'>${group.sketches.map((sketch) => `<button type="button" class="sketch-drawer-variant${sketch.sketch_id === current?.sketch_id ? ' active' : ''}${sketch.decision === 'keep' ? ' selected' : ''}" data-sketch-drawer-sketch="${esc(sketch.sketch_id)}" aria-pressed="${sketch.sketch_id === current?.sketch_id}"><span><img loading="lazy" decoding="async" alt="" data-sketch-drawer-thumb="${esc(sketch.sketch_id)}"></span><strong>${esc(sketch.title)}</strong><small>${(sketch.decision === 'keep' ? window.DevCoordinatorI18n.markup("sketches.selected_57fd7a") : (sketch.decision === 'reject' ? window.DevCoordinatorI18n.markup("sketches.rejected_aea4a0") : window.DevCoordinatorI18n.markup("sketches.undecided_00cc36")))}</small></button>`).join('')}</div></div>
      <section class="sketch-drawer-decision"><div><h3><span data-i18n="sketches.current_option_f4a3f8">Current option</span></h3><p class="muted">${current?.title ? esc(current?.title) : window.DevCoordinatorI18n.markup("sketches.option_unavailable_d03e4e")}</p></div><div class="actions">${['keep','reject','undecided'].map((decision) => `<button type="button" class="btn btn-small${current?.decision === decision ? ' active' : ''}" data-sketch-drawer-decision="${decision}" data-sketch-drawer-sketch="${esc(current?.sketch_id || '')}">${(decision === 'keep' ? window.DevCoordinatorI18n.markup("sketches.select_2a7802") : (decision === 'reject' ? window.DevCoordinatorI18n.markup("sketches.reject_ab604a") : window.DevCoordinatorI18n.markup("sketches.leave_undecided_65c5c0")))}</button>`).join('')}</div></section>
      <section class="sketch-drawer-comments"><div class="sketch-drawer-section-title"><h3><span data-i18n="sketches.comments_355f79">Comments</span></h3><span>${annotations.length}</span></div><div class="sketch-drawer-comment-list">${comments}</div><form data-sketch-comment-form><label class="f" for="sketch-comment-body">${window.DevCoordinatorI18n.markup("sketches.selectedOptionContext", { count: commentTargets })}<textarea id="sketch-comment-body" name="body" rows="3" maxlength="2000" placeholder="Explain why you chose this option or set of options." data-i18n-attrs='{"placeholder":"sketches.explain_why_you_chose_this_option_or_set_of_opti_15e84b"}'></textarea></label><button class="btn btn-primary" type="submit" data-sketch-add-comment>${commentLabel}</button></form></section>
      <footer class="sketch-set-drawer-foot"><a class="btn btn-small" href="${esc(sketchRoute(repositoryId, null, current?.sketch_id))}"><span data-i18n="sketches.open_full_review_canvas_26fc95">Open full review canvas</span></a><span class="muted"><span data-i18n="sketches.selections_save_as_keep_reject_or_undecided_e5bbd4">Selections save as Keep, Reject, or Undecided.</span></span></footer>
    </section>
  </div>`;
}

function renderSketchGalleryPage(repositoryId, sketches, activeSet = null, activeSketchId = null) {
  state.sketchGalleryData = sketches || [];
  state.sketchRepositoryId = repositoryId;
  state.sketchDrawerSet = activeSet;
  state.sketchDrawerSketchId = activeSketchId || (activeSet ? sketchSetGroups(sketches).find((group) => group.key === activeSet)?.sketches[0]?.sketch_id : null);
  const groups = sketchSetGroups(sketches);
  const selectedCount = sketches.filter((sketch) => sketch.decision === 'keep').length;
  const activeGroup = groups.find((group) => group.key === state.sketchDrawerSet);
  document.body.classList.toggle('sketch-drawer-open', Boolean(activeGroup));
  main.innerHTML = `<section class="sketch-gallery-page" data-ui-region="sketches-primary"><header class="sketch-gallery-heading"><div><h1><span data-i18n="sketches.sketches_a56d78">Sketches</span></h1><p class="muted"><span data-i18n="sketches.review_generated_options_by_set_then_select_one__020509">Review generated options by set, then select one or several to carry forward.</span></p></div><div class="sketch-selection-mode" role="group" aria-label="Selection mode" data-i18n-attrs='{"aria-label":"sketches.selection_mode_1935ae"}'><span><span data-i18n="sketches.selection_mode_1935ae">Selection mode</span></span><button type="button" class="seg${state.sketchSelectionMode === 'single' ? ' active' : ''}" data-sketch-mode="single" aria-pressed="${state.sketchSelectionMode === 'single'}"><span data-i18n="sketches.single_choice_45a76a">Single choice</span></button><button type="button" class="seg${state.sketchSelectionMode === 'multiple' ? ' active' : ''}" data-sketch-mode="multiple" aria-pressed="${state.sketchSelectionMode === 'multiple'}"><span data-i18n="sketches.choose_multiple_b76dc5">Choose multiple</span></button></div></header><div class="sketch-toolbar"><strong>${window.DevCoordinatorI18n.markup("sketches.value5_sketches_value6_selected_87bdd8", {value5: sketches.length, value6: selectedCount})}</strong><span class="muted">${esc(groups.length)} ${groups.length === 1 ? window.DevCoordinatorI18n.markup("sketches.set_6ee0eb") : window.DevCoordinatorI18n.markup("sketches.sets_82c6db")}</span><button class="btn btn-small" type="button" disabled title="Sketches are published by a registered skill" data-i18n-attrs='{"title":"sketches.sketches_are_published_by_a_registered_skill_a2ac72"}'><span data-i18n="sketches.publish_from_a_skill_f33a0e">Publish from a skill</span></button></div>${groups.length ? `<div class="sketch-set-list">${groups.map((group, index) => { const summary = sketchGroupSummary(group); return `<details class="sketch-set-lane"${index < 2 ? ' open' : ''}><summary><span class="sketch-set-summary"><strong>${esc(group.key)}</strong><small>${group.sketches.length} options · ${summary.selected.length ? `${summary.selected.length} selected` : window.DevCoordinatorI18n.markup("sketches.no_selection_211f6b")}</small></span><span class="sketch-set-summary-status">${summary.selected.length ? badge(`${summary.selected.length} selected`, 'ok') : badge('No selection')}</span></summary><div class="sketch-set-lane-actions"><span class="muted">${window.DevCoordinatorI18n.markup("sketches.value6_rejected_value7_undecided_bbc778", {value6: summary.rejected.length, value7: summary.pending})}</span><button type="button" class="btn btn-small" data-sketch-review-set="${esc(group.key)}"><span data-i18n="sketches.review_set_ca242c">Review set</span></button></div><div class="sketch-set-grid">${group.sketches.map((sketch) => renderSketchCard(repositoryId, group, sketch)).join('')}</div></details>`; }).join('')}</div>` : stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_sketches_have_been_published_for_this_project_8bde12"))}${renderSketchDrawer(repositoryId, activeGroup)}</section>`;
  bindSketchGallery(repositoryId);
  if (activeGroup) requestAnimationFrame(() => $('[role="dialog"]', main)?.focus({ preventScroll: true }));
  loadSketchGalleryImages(repositoryId);
  if (activeGroup && state.sketchDrawerDetail?.sketch?.sketch_id !== state.sketchDrawerSketchId) loadSketchDrawerDetail(repositoryId, activeGroup, state.sketchDrawerSketchId);
}

async function loadSketchGalleryImages(repositoryId) {
  const images = [...main.querySelectorAll('[data-sketch-thumb], [data-sketch-drawer-thumb], [data-sketch-drawer-image]')];
  const load = (image) => {
    if (image.dataset.sketchLoaded === 'true' || image.dataset.sketchLoading === 'true') return;
    const sketch = sketchGallerySketch(image.dataset.sketchThumb || image.dataset.sketchDrawerThumb || image.dataset.sketchDrawerImage);
    if (!sketch) return;
    image.dataset.sketchLoading = 'true';
    const attempt = Number(image.dataset.sketchAttempts || 0) + 1;
    image.dataset.sketchAttempts = String(attempt);
    enqueueSketchImage(() => document.contains(image)
      ? sketchImageUrl(repositoryId, sketch)
      : Promise.reject(new ApiError('stale', 'sketch card is no longer visible'))).then((url) => {
      if (!document.contains(image)) return;
      image.src = url; image.dataset.sketchLoaded = 'true'; image.classList.remove('is-unavailable');
    }).catch(() => {
      image.classList.add('is-unavailable');
      if (attempt < 2 && document.contains(image)) {
        image.dataset.sketchLoading = '';
        window.setTimeout(() => load(image), 500 * attempt);
      }
    }).finally(() => { image.dataset.sketchLoading = ''; });
  };
  if (!('IntersectionObserver' in window)) { images.forEach(load); return; }
  const observer = new IntersectionObserver((entries) => {
    entries.forEach((entry) => { if (entry.isIntersecting) { observer.unobserve(entry.target); load(entry.target); } });
  }, { rootMargin: '480px 0px' });
  images.forEach((image) => observer.observe(image));
}

async function loadSketchDrawerDetail(repositoryId, group, sketchId) {
  if (!sketchId) return;
  state.sketchDrawerLoading = true;
  try {
    const detail = await api('design.sketch.get', { repository_id: repositoryId, sketch_id: sketchId });
    if (state.sketchDrawerSet === group.key && state.sketchDrawerSketchId === sketchId) {
      state.sketchDrawerDetail = detail;
      state.sketchDrawerLoading = false;
      renderSketchGalleryPage(repositoryId, state.sketchGalleryData || [], group.key, sketchId);
    }
  } catch (error) {
    state.sketchDrawerLoading = false;
    toast(() => window.DevCoordinatorI18n.t("common.set_review_unavailable_value1_b614ef", {value1: error.message}), 'bad');
  }
}

function bindSketchGallery(repositoryId) {
  main.querySelectorAll('[data-sketch-mode]').forEach((button) => button.addEventListener('click', () => {
    state.sketchSelectionMode = button.dataset.sketchMode;
    renderSketchGalleryPage(repositoryId, state.sketchGalleryData || [], state.sketchDrawerSet, state.sketchDrawerSketchId);
  }));
  main.querySelectorAll('[data-sketch-review-set]').forEach((button) => button.addEventListener('click', (event) => {
    event.preventDefault(); event.stopPropagation();
    openSketchDrawer(repositoryId, button.dataset.sketchReviewSet, button.dataset.sketchReviewSketch || null);
  }));
  main.querySelectorAll('[data-sketch-toggle]').forEach((button) => button.addEventListener('click', async () => {
    const sketch = sketchGallerySketch(button.dataset.sketchToggle); if (!sketch) return;
    await saveSketchDecisions(repositoryId, sketch, sketch.decision === 'keep' ? 'undecided' : 'keep', 'Updated from grouped Sketches review.');
  }));
  main.querySelectorAll('[data-sketch-drawer-close], [data-sketch-drawer-backdrop]').forEach((element) => element.addEventListener('click', (event) => {
    if (event.target !== element) return;
    closeSketchDrawer(repositoryId);
  }));
  main.querySelectorAll('[data-sketch-drawer-sketch]').forEach((button) => button.addEventListener('click', (event) => {
    if (button.dataset.sketchDrawerDecision) return;
    event.preventDefault();
    openSketchDrawer(repositoryId, state.sketchDrawerSet, button.dataset.sketchDrawerSketch);
  }));
  main.querySelectorAll('[data-sketch-drawer-decision]').forEach((button) => button.addEventListener('click', async () => {
    const sketch = sketchGallerySketch(button.dataset.sketchDrawerSketch); if (!sketch) return;
    await saveSketchDecisions(repositoryId, sketch, button.dataset.sketchDrawerDecision, 'Updated during detailed sketch-set review.');
  }));
  $('[data-sketch-comment-form]', main)?.addEventListener('submit', async (event) => {
    event.preventDefault();
    const body = String(new FormData(event.target).get('body') || '').trim();
    if (body.length < 3) return;
    const group = sketchSetGroups(state.sketchGalleryData).find((item) => item.key === state.sketchDrawerSet);
    const current = group?.sketches.find((sketch) => sketch.sketch_id === state.sketchDrawerSketchId);
    const targets = group?.sketches.filter((sketch) => sketch.decision === 'keep') || [];
    const commentTargets = targets.length ? targets : current ? [current] : [];
    const submitter = event.submitter;
    if (submitter) submitter.disabled = true;
    try {
      for (const sketch of commentTargets) await api('design.sketch.annotation.create', { repository_id: repositoryId, sketch_id: sketch.sketch_id, body, marks: [] }, false);
      toast(() => window.DevCoordinatorI18n.t("common.comment_added_to_value1_value2_f65021", {value1: commentTargets.length, value2: commentTargets.length === 1 ? 'option' : 'selected options'}), 'ok');
      if (current) {
        state.sketchDrawerDetail = await api('design.sketch.get', { repository_id: repositoryId, sketch_id: current.sketch_id });
        renderSketchGalleryPage(repositoryId, state.sketchGalleryData || [], state.sketchDrawerSet, current.sketch_id);
      }
    } catch (error) { toast(() => window.DevCoordinatorI18n.t("common.comment_failed_value1_19ac4b", {value1: error.message}), 'bad'); }
    finally { if (submitter) submitter.disabled = false; }
  });
  document.removeEventListener('keydown', sketchDrawerEscape);
  if (state.sketchDrawerSet) document.addEventListener('keydown', sketchDrawerEscape);
}

function sketchDrawerEscape(event) {
  if (event.key === 'Escape' && state.sketchDrawerSet) closeSketchDrawer(state.sketchRepositoryId);
}

function openSketchDrawer(repositoryId, setName, sketchId = null) {
  const group = sketchSetGroups(state.sketchGalleryData).find((item) => item.key === setName);
  if (!group) return;
  const currentId = sketchId || group.sketches.find((sketch) => sketch.decision === 'keep')?.sketch_id || group.sketches[0]?.sketch_id;
  history.replaceState(null, '', sketchRoute(repositoryId, setName, currentId));
  render();
}

function closeSketchDrawer(repositoryId) {
  state.sketchDrawerSet = null; state.sketchDrawerSketchId = null; state.sketchDrawerDetail = null;
  history.replaceState(null, '', sketchRoute(repositoryId));
  renderSketchGalleryPage(repositoryId, state.sketchGalleryData || []);
}

async function saveSketchDecisions(repositoryId, sketch, decision, rationale) {
  const group = sketchSetGroups(state.sketchGalleryData).find((item) => item.sketches.some((item) => item.sketch_id === sketch.sketch_id));
  const updates = [];
  if (state.sketchSelectionMode === 'single' && decision === 'keep' && group) {
    for (const other of group.sketches.filter((item) => item.sketch_id !== sketch.sketch_id && item.decision === 'keep')) updates.push({ sketch: other, decision: 'undecided' });
  }
  updates.push({ sketch, decision });
  state.sketchGalleryLoadToken += 1;
  const galleryToken = state.sketchGalleryLoadToken;
  try {
    for (const update of updates) await api('design.sketch.decision', { repository_id: repositoryId, sketch_id: update.sketch.sketch_id, expected_revision: update.sketch.decision_revision, decision: update.decision, rationale }, false);
    const result = await listAllSketches(repositoryId);
    state.sketchDrawerDetail = null;
    toast(() => window.DevCoordinatorI18n.t("common.sketch_selection_saved_3e6e70"), 'ok');
    state.sketchGalleryHasMore = false;
    state.sketchGalleryOffset = result.sketches.length;
    renderSketchGalleryPage(repositoryId, result.sketches, state.sketchDrawerSet, state.sketchDrawerSketchId);
  } catch (error) {
    if (state.sketchGalleryHasMore) void loadMoreSketches(repositoryId, galleryToken).catch(() => {});
    toast(() => window.DevCoordinatorI18n.t("common.selection_failed_value1_7204fa", {value1: error.message}), 'bad');
  }
}

const SKETCH_PAGE_SIZE = 40;
async function listSketchPage(repositoryId, offset = 0, sketchSet = null) {
  const params = {
    repository_id: repositoryId,
    limit: SKETCH_PAGE_SIZE,
    ...(sketchSet ? { sketch_set: sketchSet } : {}),
  };
  if (offset) params.offset = offset;
  return api('design.sketch.list', params);
}
async function listAllSketches(repositoryId, sketchSet = null) {
  const sketches = [];
  let offset = 0;
  let hasMore = true;
  for (let page = 0; page < 100 && hasMore; page += 1) {
    const result = await listSketchPage(repositoryId, offset, sketchSet);
    const next = result.sketches || [];
    if (!next.length && result.has_more) throw new ApiError('bad_response', 'Sketch list pagination did not advance');
    sketches.push(...next);
    offset += next.length;
    hasMore = Boolean(result.has_more);
  }
  if (hasMore) throw new ApiError('bad_response', 'Sketch list has too many pages');
  return { repository_id: repositoryId, sketches, has_more: false };
}

async function loadMoreSketches(repositoryId, token) {
  if (!state.sketchGalleryHasMore || state.sketchGalleryLoadingMore || token !== state.sketchGalleryLoadToken) return;
  state.sketchGalleryLoadingMore = true;
  try {
    const result = await listSketchPage(repositoryId, state.sketchGalleryOffset);
    const current = state.sketchGalleryData || [];
    const seen = new Set(current.map((sketch) => sketch.sketch_id));
    const next = (result.sketches || []).filter((sketch) => !seen.has(sketch.sketch_id));
    if (!next.length && result.has_more) throw new ApiError('bad_response', 'Sketch list pagination did not advance');
    state.sketchGalleryData = current.concat(next);
    state.sketchGalleryOffset += (result.sketches || []).length;
    state.sketchGalleryHasMore = Boolean(result.has_more);
    if (token === state.sketchGalleryLoadToken && state.evidenceSource === 'test') {
      renderSketchGalleryPage(repositoryId, state.sketchGalleryData, state.sketchDrawerSet, state.sketchDrawerSketchId);
    }
  } finally {
    state.sketchGalleryLoadingMore = false;
  }
  if (state.sketchGalleryHasMore && token === state.sketchGalleryLoadToken) {
    await new Promise((resolve) => requestAnimationFrame(resolve));
    return loadMoreSketches(repositoryId, token);
  }
}

function sketchSetEvidenceData(group, details) {
  const cells = group.sketches.map((sketch, index) => ({
    cell_id: sketch.sketch_id, review_cell_key: null, plan_index: index,
    target_name: sketch.title, primary_journey: group.key, state_name: sketch.title,
    requested_path: null, final_path: null,
    viewport: { name: `option-${index + 1}`, width: sketch.width, height: sketch.height, device: null },
    started_at: sketch.created_at, ended_at: sketch.created_at, duration_ms: null,
    outcome: 'checked', http_status: null, source_binding_status: 'sketch', actions: [], findings: [], review: null,
    screenshots: { viewport: { status: 'available', kind: 'viewport', image_id: sketch.sketch_id, mime: sketch.mime, size: sketch.byte_size, sha256: sketch.sha256, width: sketch.width, height: sketch.height, captured_at: sketch.created_at }, full_page: null },
    formal_run_id: `sketch-set-${group.key}`,
  }));
  const feedback = group.sketches.flatMap((sketch) => sketchFeedback(details.get(sketch.sketch_id) || { sketch, annotations: [] }));
  return {
    repository_id: group.sketches[0]?.repository_id, feedback, issues: [], image_count: group.sketches.length,
    status: 'available', run_id: group.sketches[0]?.sketch_id, worktree_id: '',
    bundles: [{ formal_run_id: `sketch-set-${group.key}`, generated_at: group.sketches[0]?.created_at,
      browser: group.sketches[0]?.source_skill, check: 'sketch-set', phase: 'design', case: group.key,
      coverage: { checked_pages: group.sketches.length, planned_pages: group.sketches.length, failed: false, readiness_eligible: false },
      cells }],
  };
}

async function viewSketchSetEvidence(repositoryId, group, activeSketchId) {
  const details = new Map(await Promise.all(group.sketches.map(async (sketch) => [sketch.sketch_id, await api('design.sketch.get', { repository_id: repositoryId, sketch_id: sketch.sketch_id })])));
  state.evidenceSource = 'sketch-set'; state.sketchRepositoryId = repositoryId; state.sketchSetGroup = group; state.sketchSetDetails = details;
  state.sketchGalleryData = group.sketches; state.sketchDetail = details.get(activeSketchId) || details.get(group.sketches[0]?.sketch_id);
  state.evidenceRunId = state.sketchDetail?.sketch?.sketch_id;
  state.evidenceRun = { run_id: state.evidenceRunId, display_name: group.key, test: group.key, status: 'available', readiness_eligible: false, started_at: group.sketches[0]?.created_at, worktree_path: '' };
  state.evidenceData = sketchSetEvidenceData(group, details); state.evidenceSteps = evidenceSteps(state.evidenceData);
  state.evidenceStepKey = state.evidenceSteps.find((step) => step.variants[0]?.cell_id === state.evidenceRunId)?.key || state.evidenceSteps[0]?.key;
  state.evidenceViewport = null; state.evidenceScreenshotKind = 'viewport'; state.evidenceSelectedFeedbackId = null;
  main.innerHTML = evidenceWorkspace(state.evidenceRun, state.evidenceData);
  refreshEvidenceSelection(); setupEvidenceLayout();
  requestAnimationFrame(() => $('#evidence-canvas', main)?.focus({ preventScroll: true }));
}

const viewSketches = guard(async (repositoryId, sketchId = null, setName = null) => {
  const query = new URLSearchParams(location.hash.split('?')[1] || '');
  if (query.has('surface') || (!sketchId && !setName)) {
    return window.DevCoordinatorSketches.mount(main, {
      repositoryId, api, esc, signal: viewAbort.signal, imageUrl: sketchImageUrl,
      openAnnotations: async (nodes, selectedId) => {
        await viewSketchSetEvidence(repositoryId, { key: nodes[0]?.sketch_set, sketches: nodes }, selectedId);
        const link = document.createElement('a'); link.className = 'btn btn-small sketch-back';
        link.href = window.DevCoordinatorSketches.route(repositoryId, query.get('surface'), selectedId);
        window.DevCoordinatorI18n.text(link, 'sketches.backToHistory'); main.prepend(link);
      },
    });
  }
  if (sketchId && setName) {
    main.innerHTML = `${pageHeading('Sketches', '#/sketches', window.DevCoordinatorI18n.t("shell.loading_ba3bbb"))}<div class="sketch-set-list">${skeleton(6)}</div>`;
    const result = await listAllSketches(repositoryId, setName);
    const group = sketchSetGroups(result.sketches).find((item) => item.key === setName);
    if (!group) { main.innerHTML = `${pageHeading('Sketches', '#/sketches')}${stateBlock('empty', window.DevCoordinatorI18n.t("evidence.sketchSetUnavailable"))}`; return; }
    return viewSketchSetEvidence(repositoryId, group, sketchId);
  }
  if (sketchId && !setName) {
    const detail = await api('design.sketch.get', { repository_id: repositoryId, sketch_id: sketchId });
    state.evidenceSource = 'sketch'; state.sketchRepositoryId = repositoryId; state.sketchDetail = detail;
    state.evidenceRunId = sketchId; state.evidenceRun = { run_id: sketchId, display_name: detail.sketch.title, test: detail.sketch.title, started_at: detail.sketch.created_at, worktree_path: '' };
    state.evidenceData = sketchEvidence(detail); state.evidenceSteps = evidenceSteps(state.evidenceData); state.evidenceStepKey = state.evidenceSteps[0]?.key; state.evidenceViewport = null; state.evidenceScreenshotKind = 'viewport';
    main.innerHTML = evidenceWorkspace(state.evidenceRun, state.evidenceData); refreshEvidenceSelection(); setupEvidenceLayout(); return;
  }
  state.evidenceSource = 'test'; state.sketchDrawerSet = setName; state.sketchDrawerSketchId = sketchId;
  main.innerHTML = `${pageHeading('Sketches', '#/sketches', 'Loading')}<div class="sketch-set-list">${skeleton(6)}</div>`;
  const result = await listSketchPage(repositoryId);
  state.sketchGalleryOffset = result.sketches.length;
  state.sketchGalleryHasMore = Boolean(result.has_more);
  state.sketchGalleryLoadToken += 1;
  const token = state.sketchGalleryLoadToken;
  renderSketchGalleryPage(repositoryId, result.sketches, setName, sketchId);
  if (state.sketchGalleryHasMore) void loadMoreSketches(repositoryId, token).catch((error) => {
    if (error.code !== 'stale') toast(() => window.DevCoordinatorI18n.t("common.error_request") + ` [${error.code}]`, 'bad');
  });
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
  if (labels[value]) return window.DevCoordinatorI18n.t('health.label_' + (labels === HEALTH_CONTAINER_LABELS ? 'container_' : 'storage_') + value);
  return String(value || '').replaceAll('_', ' ').replace(/\b\w/g, (letter) => letter.toUpperCase());
}
function healthStorageBreakdown(storage) {
  const entries = Object.entries(storage || {});
  if (!entries.length) return '<span class="muted">—</span>';
  return `<dl class="health-storage-breakdown">${entries.map(([name, value]) => `<div><dt>${window.DevCoordinatorI18n.computedMarkup(() => healthLabel(name, HEALTH_STORAGE_LABELS))}</dt><dd>${window.DevCoordinatorI18n.computedMarkup(() => bytes(value))}</dd></div>`).join('')}</dl>`;
}
const healthPage = window.DevCoordinatorHealth.create({ api, esc, bytes, pct, spark, chart, pageHeading, icon: planIcon });
const viewHealth = guard(async (sub) => sub === 'containers' ? viewContainers() : healthPage.show());


const viewContainers = guard(async () => {
  main.innerHTML = `${pageHeading('Health', '#/health', 'Containers')}${skeleton(6)}`;
  const { containers, counts } = await api('health.containers', {});
  const admin = state.who?.administrator;
  main.innerHTML = `${pageHeading('Health', '#/health', 'Containers')}<p><a href="#/health"><span data-i18n="health.health_9959ad">← Health</span></a> · ${Object.entries(counts).map(([k, v]) => `${esc(k)} ${v}`).join(' · ')}</p>${containers.length ? `<div class="tablewrap"><table><thead><tr><th><span data-i18n="health.name_identity_747c22">Name / identity</span></th><th><span data-i18n="health.state_a3b50c">State</span></th><th><span data-i18n="health.class_4f3a9b">Class</span></th><th><span data-i18n="health.repository_13d6ff">Repository</span></th><th><span data-i18n="health.deployment_test_65464e">Deployment / test</span></th><th><span data-i18n="health.caller_9bc9b8">Caller</span></th><th><span data-i18n="health.cpu_db9a4c">CPU</span></th><th><span data-i18n="health.memory_c3963a">Memory</span></th><th><span data-i18n="health.layer_ac9ffc">Layer</span></th><th><span data-i18n="health.created_d70b9e">Created</span></th><th><span data-i18n="health.ttl_76f601">TTL</span></th><th><span data-i18n="health.actions_ff8059">Actions</span></th></tr></thead><tbody>${containers.map((c) => `<tr>
    <td class="wrap"><strong>${esc(c.name)}</strong><div class="muted mono">${esc(c.id)}</div><div class="muted">${esc(c.image)}</div></td><td>${badge(c.state, c.state === 'running' ? 'ok' : '')}</td><td>${badge(c.classification, c.classification === 'unmanaged' ? 'warn' : c.classification === 'orphaned-managed' ? 'bad' : 'ok')}</td>
    <td class="mono">${esc(c.repository_id || '—')}</td><td class="mono wrap">${esc(c.deployment_id ? `${c.deployment_id}/${c.component}` : c.run_id || '—')}</td><td>${c.caller_uid ?? '—'} ${esc(c.client || '')}</td><td>${window.DevCoordinatorI18n.computedMarkup(() => pct(c.cpu_percent))}</td><td>${window.DevCoordinatorI18n.computedMarkup(() => bytes(c.memory_bytes))}</td><td>${window.DevCoordinatorI18n.computedMarkup(() => bytes(c.container_layer_bytes))}</td><td class="wrap">${esc(c.created)}</td><td>${c.ttl_seconds ?? '—'}</td>
    <td class="actions">${admin && (c.classification === 'orphaned-managed' || c.classification === 'managed-test') ? `<button class="btn btn-small btn-danger" data-cmd="health.container_remove" data-args='${esc(JSON.stringify({ container_id: c.id }))}' aria-label="${esc(window.DevCoordinatorI18n.t(c.classification === 'managed-test' ? "health.removeTestContainer" : "health.removeOrphanedContainer", {name:c.name}))}" data-i18n-attrs='${esc(JSON.stringify({"aria-label":c.classification === 'managed-test' ? "health.removeTestContainer" : "health.removeOrphanedContainer"}))}' data-i18n-args='${esc(JSON.stringify({name:c.name}))}'>${window.DevCoordinatorI18n.markup(c.classification === 'managed-test' ? "health.removeTest" : "health.removeOrphaned")}</button>` : `<span class="muted">${(c.classification === 'observed-current' ? window.DevCoordinatorI18n.markup("health.controlled_via_its_deployment_59424f") : window.DevCoordinatorI18n.markup("health.decide_manually_80c52a"))}</span>`}</td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_containers_on_this_host_f76304"))}`;
  bind(main);
});

// --- Codex usage ---------------------------------------------------------
const USAGE_PHASES = ['planning', 'implementation', 'testing', 'deployment', 'reporting', 'unattributed'];
const USAGE_PHASE_LABELS = {
  planning: 'Planning', implementation: 'Implementation', testing: 'Testing',
  deployment: 'Deployment', reporting: 'Reporting', unattributed: 'Unattributed',
};

function compactNumber(value) {
  return window.DevCoordinatorI18n.number(value, { notation: 'compact', maximumFractionDigits: 1 });
}

function durationMs(value) {
  if (value == null) return '—';
  let seconds = Math.max(0, Math.round(Number(value) / 1000));
  const hours = Math.floor(seconds / 3600); seconds %= 3600;
  const minutes = Math.floor(seconds / 60); seconds %= 60;
  if (hours) return window.DevCoordinatorI18n.t('common.durationHours', { hours, minutes });
  if (minutes) return window.DevCoordinatorI18n.t('common.durationMinutes', { minutes, seconds });
  return window.DevCoordinatorI18n.t('common.durationSeconds', { seconds });
}

function utcBucket(ms, includeDate = false) {
  const date = new Date(ms);
  const time = new Intl.DateTimeFormat(window.DevCoordinatorI18n.locale, {
    hour: 'numeric', minute: '2-digit', timeZone: 'UTC',
  }).format(date);
  if (!includeDate) return time;
  const day = new Intl.DateTimeFormat(window.DevCoordinatorI18n.locale, {
    month: 'short', day: 'numeric', timeZone: 'UTC',
  }).format(date);
  return `${day}, ${time}`;
}

function usageSnapshotText(coverage) {
  const snapshot = coverage?.snapshot;
  if (!snapshot) return '';
  if (snapshot.refreshing && snapshot.progress_total > 0) {
    const percent = Math.min(100, Math.round((Number(snapshot.progress_completed || 0) / Number(snapshot.progress_total)) * 100));
    return `Indexing usage cache · ${percent}%`;
  }
  if (!snapshot.updated_at_ms) return (snapshot.refreshing ? window.DevCoordinatorI18n.t("common.loading_usage_0134e9") : window.DevCoordinatorI18n.t("common.usage_unavailable_refresh_failed_a2ed62"));
  const saved = ago(new Date(snapshot.updated_at_ms).toISOString());
  if (snapshot.refresh_failed) return 'Refresh failed; showing saved usage · ' + saved;
  return (snapshot.refreshing ? 'Updating saved usage · ' : 'Usage snapshot · ') + saved;
}

function usageRebuildProgress(coverage) {
  const snapshot = coverage?.snapshot;
  const total = Number(snapshot?.progress_total);
  const completed = Number(snapshot?.progress_completed || 0);
  if (!snapshot || !Number.isFinite(total) || total <= 0 || completed >= total) return '';
  const percent = Math.min(100, Math.max(0, Math.round((completed / total) * 100)));
  const stage = String(snapshot.progress_stage || 'usage cache').replaceAll('_', ' ');
  return `<div class="usage-rebuild-progress" role="status" aria-live="polite"><div class="usage-rebuild-progress-head"><strong>Rebuilding usage cache</strong><span>${esc(stage)}</span><b>${percent}%</b></div><progress max="${total}" value="${Math.min(completed, total)}">${percent}%</progress><small>${window.DevCoordinatorI18n.formatted('number', completed)} / ${window.DevCoordinatorI18n.formatted('number', total)} records</small></div>`;
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

const usageRefreshTimers = new Map();
function continueUsageRefresh(coverages, refresh) {
  const identity = usageViewIdentity();
  const scheduled = usageRefreshTimers.get(identity);
  if (!coverages.some((coverage) => coverage?.snapshot?.refreshing)) {
    if (scheduled) window.clearTimeout(scheduled);
    usageRefreshTimers.delete(identity);
    return;
  }
  if (scheduled) return;
  const controller = viewAbort;
  const timer = window.setTimeout(() => {
    usageRefreshTimers.delete(identity);
    if (controller !== viewAbort || controller?.signal.aborted || identity !== usageViewIdentity()) return;
    refresh();
  }, 5000);
  usageRefreshTimers.set(identity, timer);
}

function coverageKind(value) {
  const coverage = value && typeof value === 'object' ? value : null;
  const stateName = coverage?.state || value;
  if (coverage?.snapshot?.refreshing) return 'indexing';
  if (coverage?.snapshot?.refresh_failed) return coverage.snapshot.updated_at_ms ? 'warn' : 'bad';
  if (coverage?.unavailable_reasons?.indexing) return 'indexing';
  if (stateName === 'unavailable' && Object.keys(coverage?.unavailable_reasons || {}).length && Object.keys(coverage.unavailable_reasons).every(reason => ['mapping_pending', 'mapping_unavailable'].includes(reason))) {
    return 'setup';
  }
  return stateName === 'complete' ? 'ok' : stateName === 'partial' ? 'warn'
    : stateName === 'unavailable' ? 'bad' : '';
}

function coverageText(coverage, compact = false) {
  if (coverage.snapshot?.refreshing || coverage.snapshot?.refresh_failed) {
    if (!coverage.snapshot.updated_at_ms) return (coverage.snapshot.refreshing ? window.DevCoordinatorI18n.t("common.loading_usage_0134e9") : window.DevCoordinatorI18n.t("common.usage_refresh_failed_51e310"));
    return (coverage.snapshot.refresh_failed ? window.DevCoordinatorI18n.t("common.saved_usage_refresh_failed_c2c289") : window.DevCoordinatorI18n.t("common.saved_usage_updating_35ffa5"));
  }
  const configured = Number(coverage.configured_collectors || 0);
  const included = Number(coverage.contributing_collectors || 0);
  if (coverage.unavailable_reasons?.indexing) return window.DevCoordinatorI18n.t("common.updating_usage_data_c2a58f");
  if (coverage.state === 'complete') {
    const total = configured || included;
    // Keep the noun inside the locale catalog. Passing the English
    // “environment/environments” as a parameter made an otherwise translated
    // coverage sentence switch languages and broke plural grammar.
    return window.DevCoordinatorI18n.t("common.all_value1_environments_included_e48e29", {value1: total});
  }
  if (coverage.state === 'partial') {
    if (compact) return window.DevCoordinatorI18n.t("common.value1_of_value2_environments_included_8432b4", {value1: included, value2: configured});
    return window.DevCoordinatorI18n.t("common.some_usage_may_be_missing_data_from_value1_of_va_4135e3", {value1: included, value2: configured});
  }
  if (coverage.state === 'unobserved') {
    return (compact ? window.DevCoordinatorI18n.t("common.no_usage_measured_19cdc5") : window.DevCoordinatorI18n.t("common.no_usage_measured_in_this_period_0fc97c"));
  }
  if (Object.keys(coverage.unavailable_reasons || {}).length && Object.keys(coverage.unavailable_reasons).every(reason => ['mapping_pending', 'mapping_unavailable'].includes(reason))) {
    return (compact ? window.DevCoordinatorI18n.t("common.not_connected_in_all_environments_a56c0e") : window.DevCoordinatorI18n.t("common.not_connected_in_every_configured_codex_environm_914a8f"));
  }
  if (coverage.state === 'unavailable') return window.DevCoordinatorI18n.t("common.usage_data_unavailable_53ff18");
  return window.DevCoordinatorI18n.t("common.some_usage_may_be_missing_data_from_value1_of_va_4135e3", {value1: included, value2: configured});
}

function coverageExplanation(coverage) {
  const introduction = window.DevCoordinatorI18n.t("common.a_codex_environment_is_a_separately_configured_l_2a80d8");
  if (coverage.unavailable_reasons?.indexing) {
    return window.DevCoordinatorI18n.t("common.value1_this_range_is_still_being_prepared_from_t_7849f1", {value1: introduction});
  }
  if (coverage.state === 'complete') {
    return window.DevCoordinatorI18n.t("common.value1_every_configured_environment_supplied_mea_86002c", {value1: introduction});
  }
  if (coverage.state === 'partial') {
    return window.DevCoordinatorI18n.t("common.value1_data_from_connected_environments_is_shown_da1958", {value1: introduction});
  }
  if (coverage.state === 'unobserved') {
    const setup = (coverage.unavailable_reasons?.mapping_pending ? window.DevCoordinatorI18n.t("common.this_repository_is_not_connected_in_every_config_1dddcf") : '');
    return window.DevCoordinatorI18n.t("common.value1_the_connected_environments_contained_no_m_aa4a8d", {value1: introduction, value2: setup});
  }
  if (Object.keys(coverage.unavailable_reasons || {}).length && Object.keys(coverage.unavailable_reasons).every(reason => ['mapping_pending', 'mapping_unavailable'].includes(reason))) {
    return window.DevCoordinatorI18n.t("common.value1_this_repository_has_not_yet_been_connecte_cbf696", {value1: introduction});
  }
  if (coverage.state === 'unavailable') {
    return window.DevCoordinatorI18n.t("common.value1_configured_environments_could_not_supply__b49a80", {value1: introduction});
  }
  return window.DevCoordinatorI18n.t("common.value1_environments_that_supplied_no_data_and_un_06eb58", {value1: introduction});
}

function bucketDataStatus(stateName) {
  if (stateName === 'complete') return window.DevCoordinatorI18n.t("common.measured_9298c8");
  if (stateName === 'partial') return window.DevCoordinatorI18n.t("common.measured_with_gaps_57d879");
  if (stateName === 'unobserved') return window.DevCoordinatorI18n.t("common.not_measured_040f78");
  if (stateName === 'unavailable') return window.DevCoordinatorI18n.t("common.unavailable_ca1844");
  return window.DevCoordinatorI18n.t("common.unknown_b764cd");
}

function coverageMark(coverage, compact = false) {
  return `<span class="usage-coverage-mark ${coverageKind(coverage)}"><i aria-hidden="true"></i>${window.DevCoordinatorI18n.computedMarkup(() => coverageText(coverage, compact))}</span>${coverage.snapshot?.updated_at_ms ? `<small class="muted">Saved ${window.DevCoordinatorI18n.computedMarkup(() => ago(new Date(coverage.snapshot.updated_at_ms).toISOString()))}</small>` : ''}`;
}

function coverageHint(coverage) {
  const hintId = 'usage-coverage-hint';
  const titleId = `${hintId}-title`;
  return `<div class="usage-coverage-line"><span class="usage-coverage-status">${coverageMark(coverage)}</span><button type="button" class="usage-coverage-hint-toggle" aria-label="Explain Codex usage data completeness" aria-haspopup="dialog" aria-expanded="false" aria-controls="${hintId}" data-usage-coverage-hint-toggle data-i18n-attrs='{"aria-label":"usage.explain_codex_usage_data_completeness_63247d"}'>${planIcon('info-circle')}</button><div class="usage-coverage-popover" id="${hintId}" role="dialog" aria-labelledby="${titleId}" tabindex="-1" data-ui-allow-overlap="Open information hint intentionally overlays dashboard content" hidden><strong id="${titleId}" data-ui-continuation-anchor><span data-i18n="usage.about_codex_environments_4a2e3e">About Codex environments</span></strong><p>${window.DevCoordinatorI18n.computedMarkup(() => coverageExplanation(coverage))}</p></div></div>`;
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
  return `<div class="usage-legend" aria-label="Work phase legend" data-i18n-attrs='{"aria-label":"usage.work_phase_legend_0886c1"}'>${USAGE_PHASES.map((phase) => `<span><i class="usage-phase-${phase}" aria-hidden="true"></i>${window.DevCoordinatorI18n.markup("common.phase_" + phase)}</span>`).join('')}</div>`;
}

function usagePhaseChart(series, totalCost = null) {
  if (!series.some((point) => point.total_tokens > 0)) {
    return stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_provider_reported_total_tokens_in_this_window_b714e4"));
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
  const allTokens = series.reduce((sum, point) => sum + Number(point.total_tokens || 0), 0);
  const overallCost = costValue(totalCost);
  const bars = series.map((point, index) => {
    const x = left + index * column + (column - barWidth) / 2;
    let y = top + ih;
    const pointTokens = Number(point.total_tokens || 0);
    const proportionalCost = overallCost != null && allTokens > 0 ? overallCost * pointTokens / allTokens : null;
    return USAGE_PHASES.map((phase) => {
      const value = Number(point.phases?.[phase] || 0);
      if (!value) return '';
      const h = (value / roundedMax) * ih; y -= h;
      const tooltip = `${utcBucket(point.bucket_start_ms, true)} · ${window.DevCoordinatorI18n.t("common.phase_" + phase)} · ${Number(value).toLocaleString(window.DevCoordinatorI18n.locale)} tokens · ${proportionalCost == null ? 'cost unavailable' : `${new Intl.NumberFormat(window.DevCoordinatorI18n.locale, { style: 'currency', currency: 'USD' }).format(proportionalCost)} proportional estimate`}`;
      return `<rect tabindex="0" role="button" data-usage-chart-point="${index}" data-usage-chart-start="${point.bucket_start_ms}" data-usage-chart-phase="${esc(phase)}" data-usage-chart-tokens="${value}" data-usage-chart-total="${pointTokens}" data-usage-chart-cost="${proportionalCost == null ? '' : proportionalCost}" x="${x.toFixed(2)}" y="${y.toFixed(2)}" width="${barWidth.toFixed(2)}" height="${Math.max(.5, h).toFixed(2)}" class="usage-phase-${phase}"><title>${esc(tooltip)}</title></rect>`;
    }).join('');
  }).join('');
  const labelEvery = Math.max(1, Math.ceil(series.length / 8));
  const labels = series.map((point, index) => {
    if (index % labelEvery && index !== series.length - 1) return '';
    const x = left + index * column + column / 2;
    return `<text x="${x.toFixed(2)}" y="${height - 13}" text-anchor="middle">${esc(utcBucket(point.bucket_start_ms, index === 0))}</text>`;
  }).join('');
  return `<div class="usage-chart-scroll" tabindex="0" aria-label="Scrollable token chart" data-i18n-attrs='{"aria-label":"usage.scrollable_token_chart_83cb1f"}'><svg class="usage-phase-chart" viewBox="0 0 ${width} ${height}" role="img" aria-labelledby="usage-chart-title usage-chart-desc"><title id="usage-chart-title" data-i18n="usage.provider_reported_total_tokens_by_work_phase_d1895d">Provider-reported total tokens by work phase</title><desc id="usage-chart-desc" data-i18n="usage.stacked_utc_time_buckets_exact_values_are_listed_993096">Stacked UTC time buckets. Exact values are listed after the charts.</desc>${grid}${bars}${labels}<text x="4" y="${top + 3}" class="usage-axis-title" data-i18n="usage.tokens_a039df">Tokens</text></svg><div class="usage-chart-tooltip" data-usage-chart-tooltip role="status" hidden></div></div>`;
}

function bindUsageChartTooltip(root) {
  const chart = root.querySelector('.usage-chart-scroll');
  const tooltip = chart?.querySelector('[data-usage-chart-tooltip]');
  if (!chart || !tooltip) return;
  const show = (point) => {
    const index = Number(point.dataset.usageChartPoint);
    const phase = point.dataset.usageChartPhase.replaceAll('_', ' ');
    const tokens = Number(point.dataset.usageChartTokens || 0);
    const total = Number(point.dataset.usageChartTotal || 0);
    const cost = point.dataset.usageChartCost === '' ? null : Number(point.dataset.usageChartCost);
    const rect = point.getBoundingClientRect();
    const box = chart.getBoundingClientRect();
    tooltip.innerHTML = `<strong>${esc(phase)}</strong><span>${esc(utcBucket(Number(point.dataset.usageChartStart), true))} · bucket ${index + 1}</span><dl><div><dt>Phase tokens</dt><dd>${window.DevCoordinatorI18n.formatted('number', tokens)}</dd></div><div><dt>Bucket tokens</dt><dd>${window.DevCoordinatorI18n.formatted('number', total)}</dd></div><div><dt>Estimated cost</dt><dd>${cost == null ? '—' : esc(costAmount({ estimated_usd_micros: Math.round(cost * 1000000), status: 'partial' }))}</dd></div></dl><small>Proportional API-equivalent allocation · hover or focus another bar</small>`;
    tooltip.hidden = false;
    const x = Math.max(8, Math.min(rect.left - box.left + rect.width / 2 - 110, box.width - 228));
    tooltip.style.left = `${x}px`;
    tooltip.style.top = `${Math.max(8, rect.top - box.top - tooltip.offsetHeight - 10)}px`;
  };
  chart.querySelectorAll('[data-usage-chart-point]').forEach((point) => {
    point.addEventListener('pointerenter', () => show(point));
    point.addEventListener('focus', () => show(point));
    point.addEventListener('click', () => show(point));
    point.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') { tooltip.hidden = true; point.blur(); }
    });
  });
  chart.addEventListener('pointerleave', () => { if (!chart.contains(document.activeElement)) tooltip.hidden = true; });
}

function usageMetric(label, value, detail = '') {
  return `<div class="usage-metric"><div class="k">${window.DevCoordinatorI18n.computedMarkup(() => window.DevCoordinatorI18n.label(label))}</div><div class="v">${esc(value)}</div>${detail ? `<div class="usage-metric-detail">${detail}</div>` : ''}</div>`;
}

function costValue(cost) {
  if (!cost || cost.estimated_usd_micros == null) return null;
  return Number(cost.estimated_usd_micros) / 1000000;
}

function costAmount(cost, options = {}) {
  const value = costValue(cost);
  if (value == null || cost?.status === 'unavailable') return '—';
  return new Intl.NumberFormat(window.DevCoordinatorI18n.locale, {
    style: 'currency', currency: 'USD', minimumFractionDigits: value < 0.01 ? 4 : 2,
    maximumFractionDigits: value < 0.01 ? 4 : 2, ...options,
  }).format(value);
}

function costBasis(cost) {
  if (!cost || cost.status === 'unavailable') return 'API-equivalent estimate unavailable';
  return cost.status === 'partial' ? 'API-equivalent estimate · partial rate coverage' : 'API-equivalent estimate';
}

function costRateCardLabel(cost) {
  const refs = Array.isArray(cost?.rate_card_refs) ? cost.rate_card_refs.filter(Boolean) : [];
  return refs.length ? refs.join(', ') : (cost?.rate_card_ref || 'rate card unavailable');
}

function costComponents(cost) {
  if (!cost) return '';
  const parts = [
    ['Input', cost.input_usd_micros],
    ['Cache read', cost.cached_input_usd_micros],
    ['Cache write', cost.cache_write_usd_micros],
    ['Output', cost.output_usd_micros],
  ].filter(([, value]) => value != null).map(([label, value]) => `${label} ${costAmount({ ...cost, estimated_usd_micros: value })}`);
  return parts.join(' · ');
}

function activityRows(data) {
  const first = data.activities.slice(0, 9);
  for (const special of ['accounting_overhead', 'mixed', 'unknown']) {
    const row = data.activities.find((item) => item.activity === special);
    if (row && !first.includes(row)) first.push(row);
  }
  if (!first.length) return "<p class=\"muted\"><span data-i18n=\"usage.no_classified_token_activity_in_this_window_f460d0\">No classified token activity in this window.</span></p>";
  return `<div class="usage-activity-table" role="table" aria-label="Activity breakdown" data-i18n-attrs='{"aria-label":"usage.activity_breakdown_7ebc89"}'><div class="usage-activity-head" role="row"><span><span data-i18n="usage.work_activity_bf3178">Work activity</span></span><span><span data-i18n="usage.total_tokens_e7601c">Total tokens</span></span><span>API-equivalent estimate</span><span>%</span></div>${first.map((row) => {
    const label = window.DevCoordinatorI18n.activity(row.activity);
    const percent = row.share == null ? null : row.share * 100;
    return `<div class="usage-activity-row" role="row"><span class="usage-activity-name"><i class="usage-phase-${esc(row.phase)}" aria-hidden="true"></i>${window.DevCoordinatorI18n.computedMarkup(() => window.DevCoordinatorI18n.activity(row.activity))}</span><span class="usage-activity-bar"><i class="usage-phase-${esc(row.phase)}" style="width:${Math.max(1, percent || 0).toFixed(1)}%"></i></span><strong title="" ${window.DevCoordinatorI18n.computedAttribute("title", () => Number(row.total_tokens).toLocaleString(window.DevCoordinatorI18n.locale))}>${window.DevCoordinatorI18n.formatted('number', row.total_tokens, { notation: 'compact', maximumFractionDigits: 1 })}</strong><strong class="usage-cost-value" title="${esc(costBasis(row.cost))}">${esc(costAmount(row.cost))}</strong><span>${percent == null ? '—' : `${percent.toFixed(1)}%`}</span></div>`;
  }).join('')}</div>`;
}

function outcomeCostPulse(data) {
  const rows = [...(data.outcomes || [])].sort((a, b) => (b.cost?.estimated_usd_micros || 0) - (a.cost?.estimated_usd_micros || 0)).slice(0, 8);
  if (!rows.length) return '<p class="muted">No outcome-attributed cost measurements are available in this window.</p>';
  return `<section class="usage-outcome-pulse"><div class="usage-section-title"><div><h2>Outcome cost pulse</h2><p class="muted">Outcomes ranked by API-equivalent estimate from provider-reported usage.</p></div><details class="usage-cost-basis"><summary>Rate-card basis</summary><p>Standard public API rates are used for an API-equivalent estimate. This is operational insight, not a bill or subscription summary.</p></details></div><div class="tablewrap"><table><thead><tr><th>#</th><th>Outcome</th><th>Tokens</th><th>API-equivalent estimate</th><th>Coverage</th><th>Share</th></tr></thead><tbody>${rows.map((row, index) => { const share = data.totals?.total_tokens ? (Number(row.total_tokens) / Number(data.totals.total_tokens) * 100) : null; return `<tr><td>${index + 1}</td><td class="wrap"><strong>${esc(row.title || row.outcome_id)}</strong></td><td>${window.DevCoordinatorI18n.formatted('number', row.total_tokens, { notation: 'compact', maximumFractionDigits: 1 })}</td><td><strong>${esc(costAmount(row.cost))}</strong><small class="muted">${esc(costBasis(row.cost))}</small></td><td>${row.cost?.status === 'complete' ? '<span class="badge ok">Complete</span>' : row.cost?.status === 'partial' ? '<span class="badge warn">Partial</span>' : '<span class="badge">Unavailable</span>'}</td><td>${share == null ? '—' : `${share.toFixed(1)}%`}</td></tr>`; }).join('')}</tbody></table></div></section>`;
}

function modelCostPulse(data) {
  const rows = [...(data.models || [])]
    .sort((a, b) => (b.cost?.estimated_usd_micros || 0) - (a.cost?.estimated_usd_micros || 0)
      || Number(b.total_tokens || 0) - Number(a.total_tokens || 0))
    .slice(0, 8);
  if (!rows.length) return '';
  const palette = ['var(--chart-blue)', 'var(--chart-teal)', 'var(--chart-violet)', 'var(--line-strong)'];
  const total = costValue(data.totals?.cost);
  const first = rows[0];
  const detail = (row) => `<div class="usage-model-detail-body"><div class="usage-model-detail-head"><strong>${esc(row.model)}</strong><span>${row.cost?.status === 'complete' ? 'Complete estimate' : row.cost?.status === 'partial' ? 'Partial estimate' : 'No rate coverage'}</span></div><div class="usage-model-detail-total"><strong>${esc(costAmount(row.cost))}</strong><small>API-equivalent USD</small></div><dl><div><dt>Total tokens</dt><dd>${window.DevCoordinatorI18n.formatted('number', row.total_tokens)}</dd></div><div><dt>Requests</dt><dd>${window.DevCoordinatorI18n.formatted('number', row.model_requests)}</dd></div><div><dt>Input</dt><dd>${window.DevCoordinatorI18n.formatted('number', row.cost?.input_tokens)}</dd></div><div><dt>Cached input</dt><dd>${window.DevCoordinatorI18n.formatted('number', row.cost?.cached_input_tokens)}</dd></div><div><dt>Output</dt><dd>${window.DevCoordinatorI18n.formatted('number', row.cost?.output_tokens)}</dd></div><div><dt>Unknown observations</dt><dd>${window.DevCoordinatorI18n.formatted('number', row.cost?.unknown_observations || 0)}</dd></div></dl><p class="muted">${esc(costBasis(row.cost))}${row.cost?.rate_card_refs?.length ? ` · ${esc(row.cost.rate_card_refs.join(', '))}` : ''}</p></div>`;
  const rowsMarkup = rows.map((row, index) => {
    const knownCost = row.cost?.estimated_usd_micros != null;
    const share = total != null && knownCost ? (Number(row.cost.estimated_usd_micros) / (total * 1000000) * 100) : null;
    const bar = share == null ? 0 : Math.max(2, Math.min(100, share));
    const status = row.cost?.status === 'complete' ? 'Complete' : row.cost?.status === 'partial' ? 'Partial' : 'Unpriced';
    return `<button type="button" class="usage-model-row${index === 0 ? ' selected' : ''}" data-usage-model-index="${index}" aria-pressed="${index === 0}"><span class="usage-model-swatch" style="background:${palette[index % palette.length]}"></span><span class="usage-model-name"><strong>${esc(row.model)}</strong><small>${window.DevCoordinatorI18n.formatted('number', row.total_tokens, { notation: 'compact', maximumFractionDigits: 1 })} tokens · ${window.DevCoordinatorI18n.formatted('number', row.model_requests)} requests</small></span><span class="usage-model-bar"><i style="width:${bar.toFixed(1)}%"></i></span><strong class="usage-model-amount">${esc(costAmount(row.cost))}</strong><span class="usage-model-share">${share == null ? '—' : `${share.toFixed(1)}%`}</span><span class="badge ${status === 'Complete' ? 'ok' : status === 'Partial' ? 'warn' : ''}">${status}</span><span class="usage-model-chevron" aria-hidden="true">›</span></button>`;
  }).join('');
  return `<section class="usage-model-pulse" data-usage-model-ledger><div class="usage-section-title"><div><h2>Cost ledger by model</h2><p class="muted">Select a model to inspect token components, rate coverage, and request cost.</p></div><span class="muted">${rows.length} models</span></div><div class="usage-model-ledger-list">${rowsMarkup}</div><aside class="usage-model-detail" data-usage-model-detail>${detail(first)}</aside></section>`;
}

function bindUsageModelLedger(root, data) {
  const ledger = root.querySelector('[data-usage-model-ledger]');
  if (!ledger) return;
  const detail = ledger.querySelector('[data-usage-model-detail]');
  const rows = [...ledger.querySelectorAll('[data-usage-model-index]')];
  const models = [...(data.models || [])]
    .sort((a, b) => (b.cost?.estimated_usd_micros || 0) - (a.cost?.estimated_usd_micros || 0)
      || Number(b.total_tokens || 0) - Number(a.total_tokens || 0))
    .slice(0, rows.length);
  const render = (index) => {
    rows.forEach((row, rowIndex) => {
      const selected = rowIndex === index;
      row.classList.toggle('selected', selected);
      row.setAttribute('aria-pressed', String(selected));
    });
    const model = models[index];
    if (!model || !detail) return;
    const cost = model.cost || {};
    detail.innerHTML = `<div class="usage-model-detail-body"><div class="usage-model-detail-head"><strong>${esc(model.model)}</strong><span>${cost.status === 'complete' ? 'Complete estimate' : cost.status === 'partial' ? 'Partial estimate' : 'No rate coverage'}</span></div><div class="usage-model-detail-total"><strong>${esc(costAmount(cost))}</strong><small>API-equivalent USD</small></div><dl><div><dt>Total tokens</dt><dd>${window.DevCoordinatorI18n.formatted('number', model.total_tokens)}</dd></div><div><dt>Requests</dt><dd>${window.DevCoordinatorI18n.formatted('number', model.model_requests)}</dd></div><div><dt>Input</dt><dd>${window.DevCoordinatorI18n.formatted('number', cost.input_tokens)}</dd></div><div><dt>Cached input</dt><dd>${window.DevCoordinatorI18n.formatted('number', cost.cached_input_tokens)}</dd></div><div><dt>Output</dt><dd>${window.DevCoordinatorI18n.formatted('number', cost.output_tokens)}</dd></div><div><dt>Unknown observations</dt><dd>${window.DevCoordinatorI18n.formatted('number', cost.unknown_observations || 0)}</dd></div></dl><p class="muted">${esc(costBasis(cost))}${cost.rate_card_refs?.length ? ` · ${esc(cost.rate_card_refs.join(', '))}` : ''}</p></div>`;
  };
  rows.forEach((row, index) => row.addEventListener('click', () => render(index)));
  rows.forEach((row, index) => row.addEventListener('keydown', (event) => {
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? rows.length - 1 : (index + (event.key === 'ArrowDown' ? 1 : -1) + rows.length) % rows.length;
    rows[next].focus();
    render(next);
  }));
}

function timeRails(data) {
  const rows = [
    ['Request-to-delivery wall', data.time.request_to_delivery],
    ['Execution wall union', data.time.execution_wall],
    ['Summed agent active', data.time.summed_agent_active],
  ];
  const max = Math.max(...rows.map(([, value]) => value.measured_ms), 1);
  return `<div class="usage-rails">${rows.map(([label, value], index) => `<div class="usage-rail"><div><span>${window.DevCoordinatorI18n.computedMarkup(() => window.DevCoordinatorI18n.label(label))}</span><strong>${window.DevCoordinatorI18n.computedMarkup(() => durationMs(value.measured_ms))}</strong></div><div class="usage-rail-track"><i class="usage-rail-${index}" style="width:${((value.measured_ms / max) * 100).toFixed(1)}%"></i></div>${value.unknown_intervals ? `<span class="muted">${window.DevCoordinatorI18n.markup("usage.unknownIntervals", { count: value.unknown_intervals })}</span>` : ''}</div>`).join('')}<p class="muted usage-time-note"><span data-i18n="usage.separate_measurements_they_are_not_added_togethe_329294">Separate measurements; they are not added together.</span></p></div>`;
}

function toolOutcomeRows(data) {
  if (!data.tools.outcomes.length) return "<p class=\"muted\"><span data-i18n=\"usage.no_tool_outcomes_in_this_window_4f3fa4\">No tool outcomes in this window.</span></p>";
  const total = data.tools.outcomes.reduce((sum, row) => sum + row.count, 0);
  const order = ['completed', 'failed', 'interrupted', 'rejected', 'unknown'];
  const rows = [...data.tools.outcomes].sort((a, b) => order.indexOf(a.outcome) - order.indexOf(b.outcome));
  return `<div class="usage-outcomes">${rows.map((row) => `<div><span><i class="usage-outcome-${esc(row.outcome)}" aria-hidden="true"></i>${esc(row.outcome[0].toUpperCase() + row.outcome.slice(1))}</span><strong>${window.DevCoordinatorI18n.formatted('number', row.count)}</strong><span>${total ? `${((row.count / total) * 100).toFixed(1)}%` : '—'}</span></div>`).join('')}<div class="usage-outcome-total"><span><span data-i18n="usage.total_c9b3c3">Total</span></span><strong>${window.DevCoordinatorI18n.formatted('number', total)}</strong><span>${total ? '100%' : '—'}</span></div></div>`;
}

function exactUsageTable(data) {
  return `<section class="usage-exact"><h3><span data-i18n="usage.exact_bucket_values_a2a7fe">Exact bucket values</span></h3><div class="tablewrap"><table><thead><tr><th><span data-i18n="usage.utc_bucket_fbd8d9">UTC bucket</span></th><th><span data-i18n="usage.total_c9b3c3">Total</span></th>${USAGE_PHASES.map((phase) => `<th>${window.DevCoordinatorI18n.markup("common.phase_" + phase)}</th>`).join('')}<th><span data-i18n="usage.data_status_87d914">Data status</span></th></tr></thead><tbody>${data.series.map((point) => `<tr><td>${window.DevCoordinatorI18n.computedMarkup(() => utcBucket(point.bucket_start_ms, true))}</td><td>${window.DevCoordinatorI18n.formatted('number', Number(point.total_tokens))}</td>${USAGE_PHASES.map((phase) => `<td>${window.DevCoordinatorI18n.formatted('number', Number(point.phases?.[phase] || 0))}</td>`).join('')}<td>${badge(bucketDataStatus(point.coverage), coverageKind(point.coverage))}</td></tr>`).join('')}</tbody></table></div></section>`;
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
  const collectionCoverage = rows.find((row) => row.coverage?.snapshot)?.coverage;
  main.innerHTML = `<section data-ui-region="codex-usage-repositories"><div class="usage-collection-head">${pageHeading('Codex Usage', '#/usage')}${seg(['24h', '7d', '30d'], state.codexUsageRange, 'codex-range')}</div>${usageRebuildProgress(collectionCoverage)}${rows.length ? `<div class="tablewrap usage-collection-tablewrap"><table class="usage-collection-table"><thead><tr><th><span data-i18n="usage.repository_13d6ff">Repository</span></th><th><span data-i18n="usage.total_tokens_e7601c">Total tokens</span></th><th>API-equivalent estimate</th><th><span data-i18n="usage.model_requests_888826">Model requests</span></th><th><span data-i18n="usage.tool_calls_da5122">Tool calls</span></th><th><span data-i18n="usage.execution_time_1069e2">Execution time</span></th><th><span data-i18n="usage.data_included_217858">Data included</span></th></tr></thead><tbody>${rows.map((row) => `<tr><td data-label="Repository" data-i18n-attrs='{"data-label":"usage.repository_13d6ff"}'><a href="#/usage/${esc(row.repository_id)}"><strong>${esc(row.display_name)}</strong></a></td><td data-label="Total tokens" data-i18n-attrs='{"data-label":"usage.total_tokens_e7601c"}'>${window.DevCoordinatorI18n.formatted('number', row.total_tokens, { notation: 'compact', maximumFractionDigits: 1 })}</td><td data-label="API-equivalent estimate"><strong>${esc(costAmount(row.cost))}</strong></td><td data-label="Model requests" data-i18n-attrs='{"data-label":"usage.model_requests_888826"}'>${measured(row) && row.model_requests != null ? Number(row.model_requests).toLocaleString(window.DevCoordinatorI18n.locale) : '—'}</td><td data-label="Tool calls" data-i18n-attrs='{"data-label":"usage.tool_calls_da5122"}'>${measured(row) && row.tool_calls != null ? Number(row.tool_calls).toLocaleString(window.DevCoordinatorI18n.locale) : '—'}</td><td data-label="Execution time" data-i18n-attrs='{"data-label":"usage.execution_time_1069e2"}'>${measured(row) && row.execution_wall_ms != null ? esc(durationMs(row.execution_wall_ms)) : '—'}</td><td data-label="Data included" data-i18n-attrs='{"data-label":"usage.data_included_217858"}'>${coverageMark(row.coverage, true)}</td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_repositories_are_available_for_codex_usage_an_1d8f6d"))}</section>`;
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
    workspace.active ? {} : api('plan.overview', {}),
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
    <div class="usage-context"><div class="usage-title"><span class="usage-repo-mark" aria-hidden="true">${planIcon('focus-centered')}</span><h1>${destinationLink('Codex Usage', '#/usage')}</h1><span class="usage-slash" aria-hidden="true">/</span>${projectPicker(projects, repositoryId, (id) => `#/usage/${id}`, 'usage')}</div><div class="usage-range">${seg(['24h', '7d', '30d'], state.codexUsageRange, 'codex-range')}</div><div class="usage-coverage">${coverageHint(data.coverage)}<span class="muted">${data.coverage.snapshot ? esc(usageSnapshotText(data.coverage)) : `Data current ${data.coverage.freshest_at_ms ? ago(new Date(data.coverage.freshest_at_ms).toISOString()) : '—'}`}</span>${usageRebuildProgress(data.coverage)}</div></div>
    <div class="usage-metrics" data-ui-verify-min-content-inset="12">${usageMetric('Total tokens', compactNumber(data.totals.total_tokens), 'Provider-reported')}${usageMetric('API-equivalent estimate', costAmount(data.totals.cost), costBasis(data.totals.cost))}${usageMetric('Model requests', compactNumber(data.totals.model_requests))}${usageMetric('Cache efficiency', data.totals.input_tokens ? `${((Number(data.totals.cached_input_tokens || 0) / Number(data.totals.input_tokens)) * 100).toFixed(1)}%` : '—', 'Cached input / input tokens')}</div>
    <div class="usage-trend-row"><section class="usage-primary" data-ui-region="usage-primary-trend"><div class="usage-section-title"><h2><span data-i18n="usage.provider_reported_total_tokens_by_work_phase_d1895d">Provider-reported total tokens by work phase</span></h2></div>${phaseLegend()}${usagePhaseChart(data.series, data.totals.cost)}</section>${modelCostPulse(data)}</div>
    ${outcomeCostPulse(data)}
    <div class="usage-lower"><section><h2><span data-i18n="usage.activity_breakdown_7ebc89">Activity breakdown</span></h2>${activityRows(data)}</section><section><h2><span data-i18n="usage.time_breakdown_ca0aca">Time breakdown</span> <span class="muted"><span data-i18n="usage.separate_not_added_together_6dbbe5">(separate, not added together)</span></span></h2>${timeRails(data)}</section><section><h2><span data-i18n="usage.tool_outcomes_3a7a68">Tool outcomes</span></h2>${toolOutcomeRows(data)}</section></div>
    <details class="usage-provenance"><summary><strong><span data-i18n="usage.data_completeness_c7114d">Data completeness</span></strong><span>${window.DevCoordinatorI18n.computedMarkup(() => coverageText(data.coverage))}</span><strong><span data-i18n="usage.counting_method_7a480a">Counting method</span></strong><span><span data-i18n="usage.provider_reported_total_tokens_cached_input_and__9908f6">Provider-reported total tokens; cached input and reasoning are subsets.</span></span></summary><div><p>${window.DevCoordinatorI18n.computedMarkup(() => coverageExplanation(data.coverage))}</p><p>${(subsets ? esc(subsets) : window.DevCoordinatorI18n.markup("usage.token_subsets_unavailable_4cec86"))}</p><p class="usage-cost-basis-line"><strong>API-equivalent estimate</strong> · ${esc(costBasis(data.totals.cost))} · ${esc(costRateCardLabel(data.totals.cost))}<br><span class="muted">${esc(costComponents(data.totals.cost) || 'Component values unavailable')}</span></p><p>${window.DevCoordinatorI18n.markup("usage.schema_value19_taxonomy_value20_cdddf8", {value19: data.coverage.database_schemas.join(', ') || 'unavailable', value20: data.coverage.taxonomy_versions.join(', ') || 'unavailable'})}</p><p>${window.DevCoordinatorI18n.markup("usage.value21_environments_that_supplied_no_data_and_u_cbbc39", {value21: data.semantics.time})}</p>${exactUsageTable(data)}</div></details>
  </section>`;
  bindProjectPicker(main);
  bindCoverageHint(main);
  bindUsageModelLedger(main, data);
  bindUsageChartTooltip(main);
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
  return new Intl.DateTimeFormat(window.DevCoordinatorI18n.locale, {
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
  return window.DevCoordinatorI18n.percent(value, { maximumFractionDigits: 0 });
}

function progressDelta(current, previous, { lowerIsBetter = false, suffix = '' } = {}) {
  if (current == null || previous == null) return "<span class=\"muted\"><span data-i18n=\"progress.comparison_unavailable_127c52\">comparison unavailable</span></span>";
  const delta = Number(current) - Number(previous);
  if (!delta) return "<span class=\"muted\"><span data-i18n=\"progress.no_change_5d69f9\">no change</span></span>";
  const good = lowerIsBetter ? delta < 0 : delta > 0;
  const sign = delta > 0 ? '+' : '';
  return `<span class="progress-delta ${good ? 'good' : 'bad'}">${sign}${window.DevCoordinatorI18n.formatted('number', delta, { notation: 'compact', maximumFractionDigits: 1 })}${esc(suffix)}</span>`;
}

function progressForecastStrip(data) {
  const f = data.forecast;
  const available = ['available', 'ready'].includes(f.state);
  const i18n = window.DevCoordinatorI18n;
  const headline = () => available ? i18n.t('progress.likelyRelease', { date: progressRange(f) }) : i18n.t('progress.releaseUnavailable');
  const confidence = () => available ? i18n.t('progress.confidence_' + (['low','medium','high'].includes(f.confidence) ? f.confidence : 'high'), { percent: i18n.percent(f.confidence_percent / 100) }) : i18n.t('progress.confidenceUnavailable');
  const note = () => available ? i18n.t('progress.provisional') : ['no_planned_release','insufficient_completion_history'].includes(f.reason) ? i18n.t('progress.reason_' + f.reason) : f.explanation;
  return `<section class="progress-forecast" data-ui-region="progress-forecast" aria-label="${esc(i18n.t('progress.release_forecast_and_forecast_quality_cd88f6'))}">
    <div class="progress-forecast-summary"><strong class="${available ? '' : 'muted'}">${i18n.computedMarkup(headline)}</strong><span class="progress-confidence${f.confidence === 'low' ? ' low' : ''}">${i18n.computedMarkup(confidence)}</span><span class="progress-remaining">${i18n.markup('progress.remainingTasks', { count: f.remaining_tasks || 0 })}</span></div>
    <div class="progress-forecast-quality"><div><strong>${i18n.markup('progress.forecast_quality_b4b700')}</strong><ul><li>${f.unestimated_tasks ? i18n.markup('progress.unestimatedTasks', { count: f.unestimated_tasks }) : i18n.markup('progress.allEstimated')}</li><li>${i18n.markup(f.target_date_recorded ? 'progress.targetRecorded' : 'progress.targetMissing')}</li></ul></div><p>${i18n.computedMarkup(note)}</p></div>
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
  if (period === 'hour') return [new Intl.DateTimeFormat(window.DevCoordinatorI18n.locale, {
    hour: 'numeric', timeZone: 'UTC',
  }).format(date)];
  return [
    new Intl.DateTimeFormat(window.DevCoordinatorI18n.locale, { weekday: 'short', timeZone: 'UTC' }).format(date),
    new Intl.DateTimeFormat(window.DevCoordinatorI18n.locale, { month: 'short', day: 'numeric', timeZone: 'UTC' }).format(date),
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
  const empty = total || incomingTotal ? '' : `<text x="${left + chartWidth / 2}" y="${baseline - 8}" text-anchor="middle" class="progress-chart-empty" data-i18n="progress.no_recorded_movement_in_this_period_59cf50">No recorded movement in this period</text>`;
  const pointTargets = series.map((point, index) => progressPointTarget(point, index, {
    x: center(index) - step / 2, y: 8, width: step, height: height - bottom - 8, period: data.period, label,
    values: [[completedLabel, values[index].toLocaleString(window.DevCoordinatorI18n.locale)], [incomingLabel, incoming[index].toLocaleString(window.DevCoordinatorI18n.locale)], ['Completed running total', cumulative[index].toLocaleString(window.DevCoordinatorI18n.locale)]],
  })).join('');
  return `<svg class="progress-pulse-chart progress-bar-line-chart" style="min-width:${width}px" viewBox="0 0 ${width} ${height}" role="group" aria-label="${esc(label)} by ${esc(data.period)}: completed above the baseline, ${esc(incomingLabel.toLowerCase())} below, with completed running total"><title>${esc(label)} by ${esc(data.period)}</title><desc data-i18n="progress.solid_bars_above_the_baseline_show_completed_wor_587698">Solid bars above the baseline show completed work. Outlined bars below show incoming work. The thin line shows the completed running total. Exact values follow the chart.</desc><line x1="${left}" x2="${width - right}" y1="${baseline}" y2="${baseline}" class="progress-grid-h progress-zero-line"/><text x="14" y="${top + 14}" class="progress-lane-title" data-ui-verify-svg-overlap="lane title stays outside plotted work">${esc(label)}</text><text x="14" y="${top + 34}" class="progress-lane-detail" data-ui-verify-svg-overlap="lane detail stays outside plotted work">${esc(detail)}</text><text x="${width - 10}" y="${top + 22}" text-anchor="end" class="progress-lane-value progress-running-${cls}">${esc(format(total))}</text><text x="${width - 10}" y="${top + 38}" text-anchor="end" class="progress-lane-detail">${esc(completedVerb)}</text><text x="${width - 10}" y="${baseline + 24}" text-anchor="end" class="progress-lane-value progress-incoming-${cls}">${esc(format(incomingTotal))}</text><text x="${width - 10}" y="${baseline + 40}" text-anchor="end" class="progress-lane-detail">${esc(incomingVerb)}</text><polyline points="${linePoints.join(' ')}" class="progress-running-line progress-running-${cls}"/>${dots}${bars}${incomingBars}${barLabels}${incomingLabels}${empty}${labels}${pointTargets}</svg>`;
}

function progressEvidenceLane(data, {
  key, label, detail, cls, format, fixedMax = null, summarize = null,
}) {
  const values = data.series.map((point) => point[key]);
  const observedValues = values.filter((value) => value != null);
  const observed = observedValues.length > 0;
  const coverage = cls === 'tests' ? data.coverage.tests : data.coverage.tokens;
  const snapshotText = cls === 'tokens' ? usageSnapshotText(coverage) : '';
  const missingReasons = Object.keys(coverage.unavailable_reasons || {});
  const noRepositoryHistory = missingReasons.length > 0
    && missingReasons.every((reason) => reason === 'mapping_unavailable');
  const unavailable = (coverage.snapshot?.refreshing || coverage.snapshot?.refresh_failed ? snapshotText : '') || (cls === 'tests'
    ? 'No test runs recorded for this period.'
    : coverage.state === 'unobserved' || noRepositoryHistory ? 'No token data recorded for this period.' : 'Some token data is missing.');
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
      ? [[label, new Intl.NumberFormat('en-US', { style: 'percent', maximumFractionDigits: 2 }).format(value)], ['Tests passed', Number(data.series[index].tests_passed).toLocaleString(window.DevCoordinatorI18n.locale)], ['Recorded test runs', Number(data.series[index].test_runs).toLocaleString(window.DevCoordinatorI18n.locale)]]
      : [['Tokens', Number(value).toLocaleString(window.DevCoordinatorI18n.locale)], ['Token data', bucketDataStatus(data.series[index].token_coverage)]],
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
  if (!data.series.length) return stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_progress_buckets_in_this_period_9c9745"));
  return `<div class="progress-chart-legend"><span><i class="progress-legend-bar" aria-hidden="true"></i><span data-i18n="progress.green_tasks_087f3a">Green = tasks</span></span><span><i class="progress-legend-bar progress-legend-lines" aria-hidden="true"></i><span data-i18n="progress.blue_planned_lines_adfe4a">Blue = planned lines</span></span><span><i class="progress-legend-bar progress-legend-incoming" aria-hidden="true"></i><span data-i18n="progress.solid_above_completed_outlined_below_incoming_333281">Solid above = completed · outlined below = incoming</span></span><span><i class="progress-legend-line" aria-hidden="true"></i><span data-i18n="progress.line_completed_running_total_e033cd">Line = completed running total</span></span></div><div class="progress-chart-scroll" tabindex="0" aria-label="Scrollable daily progress charts" data-i18n-attrs='{"aria-label":"progress.scrollable_daily_progress_charts_8b0994"}'><div class="progress-chart-canvas">${progressBarLineLane(data, { key: 'tasks_completed', incomingKey: 'tasks_created', cumulativeKey: 'tasks_cumulative', label: 'Tasks finished and created', completedLabel: 'Tasks finished', incomingLabel: 'Tasks created', detail: 'Finished above · created below', cls: 'tasks', format: compactNumber })}${progressBarLineLane(data, { key: 'planned_lines_completed', incomingKey: 'planned_lines_added', cumulativeKey: 'lines_cumulative', label: 'Planned lines completed and added', completedLabel: 'Planned lines completed', incomingLabel: 'Planned lines added', detail: 'Completed above · added below', cls: 'lines', format: compactNumber })}${progressEvidenceLane(data, { key: 'test_pass_rate', label: 'Test pass rate', detail: 'Recorded terminal runs', cls: 'tests', format: progressPercent, fixedMax: 1 })}${progressEvidenceLane(data, { key: 'total_tokens', label: 'Token use', detail: 'Provider tokens · selected period', cls: 'tokens', format: compactNumber, summarize: (items) => items.reduce((sum, value) => sum + Number(value), 0) })}<p class="progress-missing-note"><span data-i18n="progress.missing_information_stays_blank_and_is_never_cou_f03048">Missing information stays blank and is never counted as zero.</span></p></div></div>`;
}

function progressReleaseWork(data, repositoryId) {
  const rows = data.release_work.slice(0, 5);
  const title = data.forecast.release ? 'progress.workInRelease' : 'progress.openPlanWork';
  if (!rows.length) return `<section class="progress-release-work" data-ui-region="progress-release-work"><div class="progress-section-heading"><div><h2>${window.DevCoordinatorI18n.markup(title)}</h2><span><span data-i18n="progress.shown_in_plan_order_2e6d9e">Shown in Plan order</span></span></div></div>${stateBlock('empty', () => (data.forecast.release ? window.DevCoordinatorI18n.t("common.no_open_work_remains_in_this_release_49931b") : window.DevCoordinatorI18n.t("common.no_open_work_is_recorded_58a7e0")))}</section>`;
  const body = rows.map((task) => {
    const selected = task.task_id === state.progressSelectedTaskId;
    const estimate = task.estimated_loc == null
      ? () => window.DevCoordinatorI18n.t('progress.estimateMissing') : () => window.DevCoordinatorI18n.t('progress.estimatedLines', { count: task.estimated_loc });
    return `<button type="button" class="progress-work-row${selected ? ' selected' : ''}" data-progress-task="${esc(task.task_id)}" aria-pressed="${selected}"><span class="progress-work-title"><strong>${esc(task.title)}</strong><small>${planBadge(task.status)} <span>${window.DevCoordinatorI18n.computedMarkup(estimate)}</span>${task.reopened ? ` ${badge('reopened', 'warn')}` : ''}${task.elaboration_needed ? ` ${badge('elaboration needed', 'warn')}` : ''}</small></span><span class="ti ti-chevron-right" aria-hidden="true"></span></button>`;
  }).join('');
  return `<section class="progress-release-work" data-ui-region="progress-release-work"><div class="progress-section-heading"><div><h2>${window.DevCoordinatorI18n.markup(title)}</h2><span><span data-i18n="progress.shown_in_plan_order_2e6d9e">Shown in Plan order</span></span></div></div><div class="progress-work-column-label"><span data-i18n="progress.task_4bc74b">Task</span></div>${body}<footer>${window.DevCoordinatorI18n.markup("progress.openTasksShown", { shown: rows.length, total: data.release_work.length })} <a href="#/plan/${esc(repositoryId)}"><span data-i18n="progress.open_full_plan_303994">Open full plan →</span></a></footer></section>`;
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
  return `<section class="progress-comparison" aria-labelledby="progress-comparison-title"><h2 id="progress-comparison-title">${window.DevCoordinatorI18n.markup("progress.compared_with_the_previous_matching_period_value_8b4a50", {value1: previousRange})}</h2><div>${items.map(([label, now, before, delta]) => `<article><span>${window.DevCoordinatorI18n.computedMarkup(() => window.DevCoordinatorI18n.label(label))}</span><strong>${esc(now)}</strong><small>${window.DevCoordinatorI18n.markup("progress.previous_value3_11ac0f", {value3: before})}</small>${delta}</article>`).join('')}</div></section>`;
}

function progressExactTable(data) {
  return `<details class="progress-exact"><summary><span data-i18n="progress.exact_values_and_counting_method_d3025e">Exact values and counting method</span></summary><div class="tablewrap"><table><thead><tr><th><span data-i18n="progress.utc_bucket_fbd8d9">UTC bucket</span></th><th><span data-i18n="progress.tasks_done_56a9a5">Tasks done</span></th><th><span data-i18n="progress.tasks_created_df2f3a">Tasks created</span></th><th><span data-i18n="progress.planned_lines_done_bbbaf4">Planned lines done</span></th><th><span data-i18n="progress.planned_lines_added_ef2d7c">Planned lines added</span></th><th><span data-i18n="progress.tests_e5c9d7">Tests</span></th><th><span data-i18n="progress.pass_rate_dec029">Pass rate</span></th><th><span data-i18n="progress.total_tokens_e7601c">Total tokens</span></th><th><span data-i18n="progress.token_data_fffeb5">Token data</span></th></tr></thead><tbody>${data.series.map((point) => `<tr><td>${window.DevCoordinatorI18n.computedMarkup(() => utcBucket(point.bucket_start_ms, true))}</td><td>${point.tasks_completed}</td><td>${point.tasks_created}</td><td>${window.DevCoordinatorI18n.formatted('number', Number(point.planned_lines_completed))}</td><td>${window.DevCoordinatorI18n.formatted('number', Number(point.planned_lines_added))}</td><td>${point.test_runs}</td><td>${point.test_pass_rate == null ? '—' : progressPercent(point.test_pass_rate)}</td><td>${point.total_tokens == null ? '—' : Number(point.total_tokens).toLocaleString(window.DevCoordinatorI18n.locale)}</td><td>${window.DevCoordinatorI18n.computedMarkup(() => bucketDataStatus(point.token_coverage))}</td></tr>`).join('')}</tbody></table></div><p>${window.DevCoordinatorI18n.markup("progress.countingSemantics")}</p></details>`;
}

function renderProgressDashboard(data, projects, repositoryId) {
  const visibleWork = data.release_work.slice(0, 5);
  if (!visibleWork.some((task) => task.task_id === state.progressSelectedTaskId)) {
    state.progressSelectedTaskId = visibleWork[0]?.task_id || null;
  }
  const selected = data.release_work.find((task) => task.task_id === state.progressSelectedTaskId) || null;
  const periodLabels = { hour: 'Hour', day: 'Day', week: 'Week' };
  const context = `<header class="progress-context" data-ui-region="progress-context"><div class="progress-identity"><h1>${destinationLink('Progress', '#/progress')}</h1><span aria-hidden="true">/</span>${projectPicker(projects, repositoryId, (id) => `#/progress/${id}`, 'progress')}</div><output class="progress-window">${window.DevCoordinatorI18n.computedMarkup(() => progressDate(data.window.start_ms))} – ${window.DevCoordinatorI18n.computedMarkup(() => progressDate(data.window.end_ms, true))} · UTC</output><div class="progress-period">${seg(['hour', 'day', 'week'], state.progressPeriod, 'progress-period', (period) => periodLabels[period])}</div><div class="progress-actions"><button class="btn btn-primary" type="button" data-progress-open-task${selected ? '' : ' disabled'}><span data-i18n="progress.open_selected_in_plan_a6478f">Open selected in plan</span></button><a href="#/plan/${esc(repositoryId)}"><span data-i18n="progress.open_full_plan_303994">Open full plan →</span></a></div></header>`;
  const workspace = `<div class="progress-workspace" data-ui-region="progress-primary"><section class="progress-pulse"><div class="progress-section-heading"><div><h2><span data-i18n="progress.daily_progress_87dd95">Daily progress</span></h2><span>${window.DevCoordinatorI18n.markup("progress.aligned_" + state.progressPeriod)}</span></div></div>${progressPulseChart(data)}</section>${progressReleaseWork(data, repositoryId)}</div>`;
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
  main.innerHTML = `<section data-ui-region="progress-repositories">${pageHeading('Progress', '#/progress')}${repositories.length ? `<div class="tablewrap"><table><thead><tr><th><span data-i18n="progress.repository_13d6ff">Repository</span></th><th><span data-i18n="progress.next_release_a90ad0">Next release</span></th><th><span data-i18n="progress.plan_progress_bfa53d">Plan progress</span></th><th><span data-i18n="progress.open_tasks_87cfa1">Open tasks</span></th><th></th></tr></thead><tbody>${repositories.map((row) => `<tr><td><a href="#/progress/${esc(row.repository_id)}"><strong>${esc(row.display_name)}</strong></a></td><td>${row.next_release ? `${esc(row.next_release.name)} ${planBadge(row.next_release.status)}` : "<span class=\"muted\"><span data-i18n=\"progress.no_release_planned_4f4fea\">No release planned</span></span>"}</td><td>${row.planned_lines_total ? `${meter(row.planned_lines_done / row.planned_lines_total)}<span class="muted">${window.DevCoordinatorI18n.computedMarkup(() => locN(row.planned_lines_done))} of ${window.DevCoordinatorI18n.computedMarkup(() => locN(row.planned_lines_total))} planned lines done</span>` : "<span class=\"muted\"><span data-i18n=\"progress.nothing_sized_yet_534954\">Nothing sized yet</span></span>"}</td><td>${row.open_tasks}</td><td><a class="btn btn-small" href="#/progress/${esc(row.repository_id)}"><span data-i18n="progress.view_progress_c2e39c">View progress</span></a></td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_repositories_are_available_for_progress_analy_994af5"))}</section>`;
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
  main.innerHTML = `${pageHeading('Bugs', '#/bugs')}<h2><span data-i18n="bugs.open_bugs_dce3d4">Open bugs</span></h2>${bugs.length ? `<div class="tablewrap"><table><thead><tr><th><span data-i18n="bugs.component_ce54f0">Component</span></th><th><span data-i18n="bugs.summary_8e76a9">Summary</span></th><th><span data-i18n="bugs.expected_actual_2457a7">Expected / actual</span></th><th><span data-i18n="bugs.steps_1de3df">Steps</span></th><th><span data-i18n="bugs.seen_8894cf">Seen</span></th><th><span data-i18n="bugs.correlations_f54dae">Correlations</span></th><th></th></tr></thead><tbody>${bugs.map((b) => `<tr><td>${esc(b.component)}</td><td class="wrap"><strong>${esc(b.summary)}</strong><div class="muted mono">${esc(b.bug_id)} · ${esc(b.reporter)}</div></td><td class="wrap">${esc(b.expected)}<br><span class="muted">${esc(b.actual)}</span></td><td class="wrap">${esc(b.steps)}</td><td>${b.occurrences}× · ${window.DevCoordinatorI18n.computedMarkup(() => ago(b.last_seen_at))}</td><td class="mono wrap">${esc(Object.entries(b.correlations || {}).map(([k, v]) => `${k}=${v}`).join(' ') || '—')}</td><td><button class="btn btn-small" data-cmd="bug.close" data-args='${esc(JSON.stringify({ bug_id: b.bug_id }))}'><span data-i18n="bugs.close_310ff2">close</span></button></td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_open_bugs_ca3d5d"))}
    <h2><span data-i18n="bugs.report_a_bug_f75e0b">Report a bug</span></h2><form class="inline" id="bug-form">${['component', 'summary', 'expected', 'actual', 'steps'].map((f) => `<label class="f">${window.DevCoordinatorI18n.markup("bugs." + ({component:"component_ce54f0",summary:"summary_8e76a9",expected:"expected",actual:"actual",steps:"steps_1de3df"}[f]))}<${f === 'steps' ? 'textarea' : 'input'} name="${f}" required ${f === 'steps' ? '></textarea>' : '>'}</label>`).join('')}<button class="btn" type="submit"><span data-i18n="bugs.report_b6ce78">Report</span></button></form><p class="muted"><span data-i18n="bugs.bounded_atomic_records_only_no_secrets_raw_logs__2abc9d">Bounded atomic records only: no secrets, raw logs, or private paths.</span></p>`;
  bind(main);
  $('#bug-form').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const btn = ev.target.querySelector('button');
    const args = Object.fromEntries(new FormData(ev.target).entries());
    await act(btn, 'bug.report', args, () => render());
  });
});

// --- Administration ------------------------------------------------------
function adminRole(role) {
  return role === 'access' ? window.DevCoordinatorI18n.t('admin.roleAccess') : ['viewer', 'operator', 'administrator'].includes(role) ? window.DevCoordinatorI18n.t('common.status_' + role) : role;
}

const viewAdmin = guard(async () => {
  main.innerHTML = `${pageHeading('Administration', '#/admin')}${skeleton(6)}`;
  const [users, deployments, telegram] = await Promise.all([api('user.list', {}), api('deployment.list', {}), api('telegram.list', {})]);
  const depOptions = deployments.deployments.map((d) => `<option value="${esc(d.deployment_id)}">${esc(d.name)}@${esc(d.source)}</option>`).join('');
  const roleOptions = users.roles.map((r) => `<option value="${esc(r)}" data-i18n="${r === 'access' ? 'admin.roleAccess' : 'common.status_' + esc(r)}">${esc(r)}</option>`).join('');
  main.innerHTML = `${pageHeading('Administration', '#/admin')}
    <h2><span data-i18n="admin.users_6b0cc9">Users</span></h2>${users.users.length ? `<div class="tablewrap"><table><thead><tr><th><span data-i18n="admin.e_mail_2550d9">E-mail</span></th><th><span data-i18n="admin.administrator_e7d3e7">Administrator</span></th><th><span data-i18n="admin.grants_b3e1c9">Grants</span></th><th><span data-i18n="admin.last_seen_21fd79">Last seen</span></th><th></th></tr></thead><tbody>${users.users.map((u) => `<tr><td class="wrap mono">${esc(u.email)}</td><td>${u.administrator ? badge('administrator', 'ok') : ''}</td><td class="wrap">${u.grants.map((g) => `<span class="badge">${window.DevCoordinatorI18n.computedMarkup(() => adminRole(g.role))}</span> <span class="mono">${esc(g.deployment_id)}</span> <button class="btn btn-small" data-cmd="grant.remove" data-args='${esc(JSON.stringify({ email: u.email, deployment_id: g.deployment_id }))}' ${window.DevCoordinatorI18n.computedAttribute("aria-label", () => window.DevCoordinatorI18n.t("admin.removeGrant", {role: adminRole(g.role), email: u.email}))}><span data-i18n="admin.remove_grant_8c57bf">Remove grant</span></button>`).join('<br>') || "<span class=\"muted\"><span data-i18n=\"admin.none_140bed\">none</span></span>"}</td><td>${window.DevCoordinatorI18n.computedMarkup(() => ago(u.last_seen_at))}</td><td><button class="btn btn-small btn-danger" data-cmd="user.remove" data-args='${esc(JSON.stringify({ email: u.email }))}' ${window.DevCoordinatorI18n.computedAttribute("aria-label", () => window.DevCoordinatorI18n.t("admin.removeUser", {email: u.email}))}><span data-i18n="admin.remove_user_84a18f">Remove user</span></button></td></tr>`).join('')}</tbody></table></div>` : stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_users_5bf761"))}
    <form class="inline" id="grant-form"><label class="f"><span data-i18n="admin.e_mail_39e074">e-mail</span><input name="email" required></label><label class="f"><span data-i18n="admin.deployment_aee50b">deployment</span><select name="deployment_id" required>${depOptions}</select></label><label class="f"><span data-i18n="admin.role_4b168d">role</span><select name="role">${roleOptions}</select></label><button class="btn" type="submit"><span data-i18n="admin.set_grant_35d1bf">Set grant</span></button></form>
    <h2><span data-i18n="admin.invitations_14210e">Invitations</span></h2>${users.invitations.length ? `<div class="tablewrap"><table><thead><tr><th><span data-i18n="admin.e_mail_2550d9">E-mail</span></th><th><span data-i18n="admin.administrator_e7d3e7">Administrator</span></th><th><span data-i18n="admin.grants_b3e1c9">Grants</span></th><th><span data-i18n="admin.expires_f6725f">Expires</span></th><th></th></tr></thead><tbody>${users.invitations.map((i) => `<tr><td class="mono wrap">${esc(i.email)}</td><td>${i.administrator ? window.DevCoordinatorI18n.markup("admin.yes_8a7988") : ''}</td><td class="wrap mono">${window.DevCoordinatorI18n.computedMarkup(() => i.grants.map((g) => `${adminRole(g.role)}:${g.deployment_id}`).join(' ') || '—')}</td><td>${esc(i.expires_at)}</td><td><button class="btn btn-small" data-cmd="user.remove" data-args='${esc(JSON.stringify({ email: i.email }))}'><span data-i18n="admin.revoke_777ac4">revoke</span></button></td></tr>`).join('')}</tbody></table></div>` : "<p class=\"muted\"><span data-i18n=\"admin.no_outstanding_invitations_2642d9\">No outstanding invitations.</span></p>"}
    <form class="inline" id="invite-form"><label class="f"><span data-i18n="admin.e_mail_39e074">e-mail</span><input name="email" type="email" required></label><label class="f"><span data-i18n="admin.deployment_aee50b">deployment</span><select name="deployment_id"><option value="" data-i18n="admin.none_552e42">(none)</option>${depOptions}</select></label><label class="f"><span data-i18n="admin.role_4b168d">role</span><select name="role">${roleOptions}</select></label><label class="f"><span data-i18n="admin.administrator_4194d1">administrator</span><input type="checkbox" name="administrator"></label><button class="btn" type="submit"><span data-i18n="admin.invite_1fd9ae">Invite</span></button></form>
    <h2><span data-i18n="admin.telegram_acdd1e">Telegram</span></h2><p class="muted">${window.DevCoordinatorI18n.computedMarkup(() => window.DevCoordinatorI18n.t("admin.telegramSummary", {state: window.DevCoordinatorI18n.t(telegram.configured ? "admin.configured_201582" : "admin.not_configured_9f33f0"), pending: window.DevCoordinatorI18n.number(telegram.outbox_pending), time: ago(telegram.last_poll_at)}))} ${telegram.last_error ? `· <span class="badge bad">${esc(telegram.last_error)}</span>` : ''}</p>
    ${telegram.chats.length ? `<div class="tablewrap"><table><thead><tr><th><span data-i18n="admin.chat_460b3a">Chat</span></th><th><span data-i18n="admin.e_mail_2550d9">E-mail</span></th><th><span data-i18n="admin.subscriptions_a151f2">Subscriptions</span></th><th></th></tr></thead><tbody>${telegram.chats.map((c) => `<tr><td>${c.chat_id} <span class="muted">${esc(c.label || '')}</span></td><td class="mono wrap">${esc(c.email)}</td><td class="wrap">${c.subscriptions.map((s) => `<span class="badge">${esc(s)}</span> <button class="btn btn-small" data-cmd="telegram.unsubscribe" data-args='${esc(JSON.stringify({ chat_id: c.chat_id, scope: s }))}' ${window.DevCoordinatorI18n.computedAttribute("aria-label", () => window.DevCoordinatorI18n.t("admin.unsubscribe", {scope:s}))}>×</button>`).join(' ') || "<span class=\"muted\"><span data-i18n=\"admin.none_140bed\">none</span></span>"}</td><td></td></tr>`).join('')}</tbody></table></div>` : "<p class=\"muted\"><span data-i18n=\"admin.no_linked_chats_send_start_to_the_bot_to_get_a_l_1b08a7\">No linked chats. Send /start to the bot to get a link code.</span></p>"}
    <form class="inline" id="link-form"><label class="f"><span data-i18n="admin.link_code_04a3f4">link code</span><input name="code" required></label><label class="f"><span data-i18n="admin.e_mail_39e074">e-mail</span><input name="email" type="email" required></label><button class="btn" type="submit"><span data-i18n="admin.link_chat_1e7f5a">Link chat</span></button></form>
    <form class="inline" id="sub-form"><label class="f"><span data-i18n="admin.chat_id_d6a2bb">chat id</span><input name="chat_id" type="number" required></label><label class="f"><span data-i18n="admin.scope_5f161c">scope</span><input name="scope" placeholder="server | deployment:&lt;id&gt; | repository:&lt;id&gt;" required></label><button class="btn" type="submit"><span data-i18n="admin.subscribe_cc0e38">Subscribe</span></button></form>
    <h2><span data-i18n="admin.server_aef7de">Server</span></h2><div id="server" class="muted">${skeleton(1)}</div>`;
  bind(main);
  const submit = (form, cmd, build) => $(form).addEventListener('submit', async (ev) => { ev.preventDefault(); const fd = new FormData(ev.target); await act(ev.target.querySelector('button'), cmd, build(fd), () => render()); });
  submit('#grant-form', 'grant.set', (fd) => ({ email: fd.get('email'), deployment_id: fd.get('deployment_id'), role: fd.get('role') }));
  submit('#invite-form', 'user.invite', (fd) => ({ email: fd.get('email'), administrator: fd.get('administrator') === 'on', grants: fd.get('deployment_id') ? [{ deployment_id: fd.get('deployment_id'), role: fd.get('role') }] : [] }));
  submit('#link-form', 'telegram.link', (fd) => ({ code: fd.get('code'), email: fd.get('email') }));
  submit('#sub-form', 'telegram.subscribe', (fd) => ({ chat_id: Number(fd.get('chat_id')), scope: fd.get('scope') }));
  try {
    const [ping, edge] = await Promise.all([api('ping', {}), fetch('/healthz').then((r) => r.json())]);
    $('#server').innerHTML = `daemon ${esc(ping.daemon_version)} · schema ${ping.schema_version} · route document generation ${edge.route_generation} (${esc(edge.source)})`;
  } catch (e) { window.DevCoordinatorI18n.bind($('#server'), () => e.message); }
});

// --- Plan (completion ledger, releases, previews) ------------------------
function locN(n) { return Number(n).toLocaleString(window.DevCoordinatorI18n.locale); }
function loc(n) { return n == null ? '' : window.DevCoordinatorI18n.t('plan.estimatedLines', {count:n, value:locN(n)}); }
function planEstimateText(row) {
  if (!row.isParent) return loc(row.task.estimated_loc) || window.DevCoordinatorI18n.t('plan.not_estimated_dbbcf5');
  return [row.subtreeLoc ? loc(row.subtreeLoc) : '', row.unsizedCount ? window.DevCoordinatorI18n.t('plan.unestimatedCount', {count:row.unsizedCount}) : ''].filter(Boolean).join(' + ');
}
function planDoneText(progress) {
  return window.DevCoordinatorI18n.t('plan.doneSummary', {done:locN(progress.done), total:locN(progress.total), percent:window.DevCoordinatorI18n.percent(progress.percent / 100, {maximumFractionDigits:0})});
}
const PLAN_BADGE = { done: 'ok', delivered: 'ok', in_progress: 'warn', requested: 'warn' };
function planBadge(status) { return badge(status, PLAN_BADGE[status] ?? ''); }
function planElaborationMark(task) {
  return `<span class="plan-elaboration-mark" data-elaboration-mark="${esc(task.task_id)}" data-ui-continuation-anchor tabindex="-1">${task.elaboration_needed ? badge('elaboration needed', 'warn') : ''}</span>`;
}
function planElaborationButton(task, context = '') {
  const requested = !!task.elaboration_needed;
  const key = requested ? 'plan.explanationRequested' : 'plan.explainTask';
  const explanation = window.DevCoordinatorI18n.t(key, {title:task.title});
  return `<button class="btn btn-small plan-elaborate ${esc(context)}" type="button" data-elaborate-task="${esc(task.task_id)}" data-ui-continuation-anchor aria-label="${esc(explanation)}" title="${esc(explanation)}" data-i18n-attrs='${esc(JSON.stringify({'aria-label':key,title:key}))}' data-i18n-args='${esc(JSON.stringify({title:task.title}))}'${requested ? ' aria-disabled="true"' : ''}>${planIcon('message-plus')}<span class="plan-elaborate-label">${window.DevCoordinatorI18n.markup(requested ? 'plan.requested_2d9e28' : 'plan.elaborateLabel')}</span></button>`;
}

const viewPlanPicker = guard(async (kind) => {
  const title = kind === 'decisions' ? 'Decisions' : 'Plan';
  const href = kind === 'decisions' ? '#/decisions' : '#/plan';
  main.innerHTML = `${pageHeading(title, href)}${skeleton()}`;
  const { repositories } = await api('plan.overview', {});
  if (!repositories.length) { main.innerHTML = `${pageHeading(title, href)}${stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_repositories_visible_to_you_40cf12"))}`; return; }
  main.innerHTML = `${pageHeading(title, href)}<p class="muted"><span data-i18n="plan.pick_a_repository_046ba6">Pick a repository.</span></p><div class="tablewrap"><table><thead><tr><th><span data-i18n="plan.repository_13d6ff">Repository</span></th><th><span data-i18n="plan.now_building_f780a4">Now building</span></th><th><span data-i18n="plan.progress_466482">Progress</span></th><th><span data-i18n="plan.open_tasks_87cfa1">Open tasks</span></th><th></th></tr></thead><tbody>${repositories.map((r) => `<tr>
    <td class="wrap"><a href="#/${kind}/${esc(r.repository_id)}"><strong>${esc(r.display_name)}</strong></a></td>
    <td class="wrap">${r.current_release ? `${esc(r.current_release.name)} ${planBadge(r.current_release.status)}` : "<span class=\"muted\"><span data-i18n=\"plan.no_releases_planned_yet_11b10b\">no releases planned yet</span></span>"}${r.preview_requested ? ` ${badge('preview requested', 'warn')}` : ''}${r.elaboration_request_count ? ` ${badge(`${r.elaboration_request_count} need explanation`, 'warn')}` : ''}</td>
    <td>${r.loc_total ? `${meter(r.loc_done / r.loc_total)}<span class="muted">done ${window.DevCoordinatorI18n.computedMarkup(() => locN(r.loc_done))} of ${window.DevCoordinatorI18n.computedMarkup(() => locN(r.loc_total))} lines</span>` : "<span class=\"muted\"><span data-i18n=\"plan.nothing_sized_yet_8fb780\">nothing sized yet</span></span>"}</td>
    <td>${r.open_tasks}</td>
    <td class="actions"><a class="btn btn-small" href="#/plan/${esc(r.repository_id)}"><span data-i18n="plan.plan_64879f">plan</span></a><a class="btn btn-small" href="#/decisions/${esc(r.repository_id)}"><span data-i18n="plan.decisions_3183d3">decisions</span></a></td></tr>`).join('')}</tbody></table></div>`;
});

// Layout is pure: leaf tasks advance a global lines-of-code cursor, parents
// span their descendants, releases group in sequence, backlog trails.
function seedPlanCollapsedDoneParents(releases, tasks, collapsed) {
  const known = new Set(releases.map((release) => release.release_id));
  const byId = new Map(tasks.map((task) => [task.task_id, task]));
  const groupOf = (task) => (task.release_id && known.has(task.release_id) ? task.release_id : null);
  const inTree = (task) => {
    const parent = byId.get(task.parent_task_id);
    return !!parent && groupOf(parent) === groupOf(task);
  };
  const children = new Map();
  for (const task of tasks) {
    if (!inTree(task)) continue;
    if (!children.has(task.parent_task_id)) children.set(task.parent_task_id, []);
    children.get(task.parent_task_id).push(task);
  }
  const depth = new Map();
  const visit = (task, level) => {
    const previous = depth.get(task.task_id);
    if (previous != null && previous <= level) return;
    depth.set(task.task_id, level);
    for (const child of children.get(task.task_id) || []) visit(child, level + 1);
  };
  for (const task of tasks.filter((item) => !inTree(item))) visit(task, 0);
  for (const task of tasks) {
    // Root-level completed rows are hidden separately. Keep completed parents
    // below the root visible as a compact context row with their descendants
    // collapsed, while allowing the user to expand them explicitly.
    if (task.status === 'done' && depth.get(task.task_id) > 0 && children.has(task.task_id)) collapsed.add(task.task_id);
  }
}

function computeGantt(releases, tasks, collapsed, explicitVisibleTaskIds = new Set()) {
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
    const rootDone = depth === 0 && t.status === 'done';
    const nestedDoneLeaf = depth > 0 && t.status === 'done' && children.length === 0;
    const explicitlyVisible = explicitVisibleTaskIds.has(t.task_id);
    const row = { task: t, depth, isParent: children.length > 0, hidden: hidden || (!explicitlyVisible && (rootDone || nestedDoneLeaf)), start: cursor, width: 0, subtreeLoc: 0, doneLoc: 0, unsizedCount: 0, isUnsized: false, collapsed: collapsed.has(t.task_id) };
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
    ? `<button class="btn btn-small" type="button" data-resize-task="${esc(task.task_id)}">${planIcon('arrow-right')}${(task.estimated_loc == null ? window.DevCoordinatorI18n.markup("plan.add_estimate_c87fb7") : window.DevCoordinatorI18n.markup("plan.resize_2956e0"))}</button>` : '';
  const actionButtons = admin ? `${planElaborationButton(task, 'plan-elaborate-tray')}${movable ? `<button class="btn btn-small" type="button" data-move-task="${esc(task.task_id)}">${planIcon('arrows-move')}${window.DevCoordinatorI18n.markup('plan.move_6ecc3d')}</button>` : ''}${estimateButton}${editable ? `<button class="btn btn-small btn-danger" data-cmd="task.update" data-args='${esc(JSON.stringify({ task_id: task.task_id, status: 'dropped' }))}' aria-label="${esc(window.DevCoordinatorI18n.t('plan.dropTaskNamed', {title:task.title}))}" data-i18n-attrs='{"aria-label":"plan.dropTaskNamed"}' data-i18n-args='${esc(JSON.stringify({title:task.title}))}'>${planIcon('trash')}${window.DevCoordinatorI18n.markup('plan.dropTask')}</button>` : ''}` : '';
  const actions = actionButtons ? `<div class="plan-selection-actions"><span class="plan-selection-label"><span data-i18n="plan.actions_ff8059">Actions</span></span><div class="actions">${actionButtons}</div></div>` : '';
  const estimateText = () => selectedRow.unsizedCount
    ? (selectedRow.isParent ? [selectedRow.subtreeLoc ? loc(selectedRow.subtreeLoc) : '', window.DevCoordinatorI18n.t('plan.unestimatedJobs', {count:selectedRow.unsizedCount})].filter(Boolean).join(' + ') : window.DevCoordinatorI18n.t('plan.not_estimated_yet_2a73ea'))
    : loc(progress.total);
  const progressBlock = selectedRow.isUnsized
    ? `<div class="plan-selection-detail"><span class="plan-selection-label"><span data-i18n="plan.size_1af851">Size</span></span><strong><span data-i18n="plan.not_estimated_yet_2a73ea">Not estimated yet</span></strong><span class="muted"><span data-i18n="plan.shown_in_the_non_proportional_chart_band_e2fd9f">Shown in the non-proportional chart band.</span></span></div>`
    : `<div class="plan-selection-detail"><span class="plan-selection-label"><span data-i18n="plan.progress_466482">Progress</span></span><strong>${window.DevCoordinatorI18n.computedMarkup(() => planDoneText(progress))}</strong><progress max="${Math.max(progress.total, 1)}" value="${progress.done}"></progress></div>`;
  return `<section class="plan-selection${state.planSelectionCollapsed ? ' collapsed' : ''}" data-ui-region="selected-task" aria-label="Selected task details" data-i18n-attrs='{"aria-label":"plan.selected_task_details_a9a7eb"}'>
    <div class="plan-selection-heading"><span class="plan-selection-grip">${planIcon('grip-vertical')}</span><strong data-ui-continuation-anchor>${esc(task.title)}</strong><span class="plan-selection-status">${planBadge(task.status)}${planElaborationMark(task)}</span><span class="muted">${window.DevCoordinatorI18n.computedMarkup(estimateText)}</span></div>
    ${progressBlock}
    <div class="plan-selection-detail plan-selection-impact"><span class="plan-selection-label"><span data-i18n="plan.why_it_matters_b8bda6">Why it matters</span></span><span>${task.impact ? esc(task.impact) : "<span class=\"muted\"><span data-i18n=\"plan.no_explanation_recorded_81eb6c\">No explanation recorded.</span></span>"}</span></div>
    <div class="plan-selection-detail"><span class="plan-selection-label"><span data-i18n="plan.release_e020e3">Release</span></span><span>${release ? esc(release.name) : window.DevCoordinatorI18n.markup("plan.not_scheduled_yet_6e4794")}</span>${release ? planBadge(release.status) : ''}</div>
    ${actions}
    <button class="plan-selection-toggle" type="button" data-plan-selection-toggle aria-expanded="${!state.planSelectionCollapsed}" aria-label="${esc(window.DevCoordinatorI18n.t(state.planSelectionCollapsed ? 'common.expandTaskDetails' : 'common.collapseTaskDetails'))}" data-i18n-attrs='${esc(JSON.stringify({"aria-label":state.planSelectionCollapsed ? 'common.expandTaskDetails' : 'common.collapseTaskDetails'}))}'>${planIcon(state.planSelectionCollapsed ? 'chevron-up' : 'chevron-down')}</button>
  </section>`;
}

const viewPlan = guard(async (repoId) => {
  const requestedTaskId = state.planRequestedTaskId;
  state.planRequestedTaskId = null;
  const repositoryChanged = state.planRepositoryId !== repoId;
  if (repositoryChanged) {
    state.planRepositoryId = repoId;
    // Collapse defaults belong to one plan. Reset them when switching
    // repositories so stale task ids cannot affect a different tree.
    state.collapsed = new Set();
    state.planExplicitVisibleTaskIds = new Set();
    state.planVisibilitySeededRepositoryId = null;
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
  if (state.planVisibilitySeededRepositoryId !== repoId) {
    seedPlanCollapsedDoneParents(model.releases, model.tasks, state.collapsed);
    state.planVisibilitySeededRepositoryId = repoId;
  }
  if (requestedTaskId) {
    state.planExplicitVisibleTaskIds = new Set();
    const byId = new Map(model.tasks.map((task) => [task.task_id, task]));
    let ancestor = byId.get(requestedTaskId);
    while (ancestor) {
      state.planExplicitVisibleTaskIds.add(ancestor.task_id);
      if (!ancestor.parent_task_id) break;
      state.collapsed.delete(ancestor.parent_task_id);
      ancestor = byId.get(ancestor.parent_task_id);
    }
  } else {
    state.planExplicitVisibleTaskIds = new Set();
  }
  const { groups, total, unsizedCount } = computeGantt(model.releases, model.tasks, state.collapsed, state.planExplicitVisibleTaskIds);
  const rows = groups.flatMap((g) => g.rows);
  const releaseById = new Map(model.releases.map((r) => [r.release_id, r]));
  const unsizedShare = unsizedCount ? (total ? 0.18 : 1) : 0;
  const sizedShare = 1 - unsizedShare;
  const pctOf = (v) => `${(total ? (v / total) * sizedShare * 100 : 0).toFixed(3)}%`;
  const unsizedLeft = `${(sizedShare * 100 + (unsizedShare ? 1 : 0)).toFixed(3)}%`;
  const unsizedWidth = `${Math.max(0, unsizedShare * 100 - (unsizedShare ? 2 : 0)).toFixed(3)}%`;
  let selectedRow = requestedTaskId ? rows.find((row) => row.task.task_id === requestedTaskId) : null;
  selectedRow ||= rows.find((row) => row.task.task_id === state.planSelectedTaskId && !row.hidden);
  if (!selectedRow) {
    selectedRow = rows.find((row) => !row.hidden && row.isParent && row.task.status !== 'done')
      || rows.find((row) => !row.hidden && row.task.status === 'in_progress')
      || rows.find((row) => !row.hidden && row.task.status !== 'done')
      || rows.find((row) => !row.hidden)
      || null;
    state.planSelectedTaskId = selectedRow?.task.task_id || null;
  }
  const doneLoc = rows.filter((row) => !row.isParent).reduce((sum, row) => sum + row.doneLoc, 0);
  const requested = model.preview_requested || [];
  const requestControl = requested.length
    ? `<span class="plan-requested">${planBadge('requested')} <span class="muted">${window.DevCoordinatorI18n.computedMarkup(() => ago(requested[0].requested_at))}</span></span>`
    : (admin ? `<button class="btn btn-primary" data-cmd="release.request" data-args='${esc(JSON.stringify({ repository_id: repoId }))}'><span data-i18n="plan.request_preview_1e9b91">Request preview</span></button>` : '');
  const requestNotice = requested.length
    ? `<div class="plan-preview-notice" role="status"><span data-i18n="plan.preview_requested_the_agent_will_put_the_current_cb6569">Preview requested. The agent will put the current work online; a link appears on the preview release when it is ready.</span></div>`
    : '';
  const elaborationCount = (model.elaboration_requests || []).length;
    const elaborationNotice = `<div class="plan-elaboration-notice" data-plan-elaboration-notice role="status"${elaborationCount ? '' : ' hidden'}>${elaborationCount ? window.DevCoordinatorI18n.markup('common.tasksNeedExplanation', { count: elaborationCount }) : ''}</div>`;

  const releaseHead = (group) => {
    const release = group.release;
    const droppable = admin && (!release || release.status !== 'delivered');
    const locationStyle = `style="--release-end:${pctOf(group.end)}"`;
    const where = release?.url && /^https:\/\//.test(release.url)
      ? `<a class="plan-release-location" ${locationStyle} href="${esc(release.url)}" target="_blank" rel="noopener"><span data-i18n="plan.open_the_app_397f86">Open the app ↗</span></a>`
      : (release?.status === 'delivered' && release.port ? `<span class="plan-release-location" ${locationStyle}>${window.DevCoordinatorI18n.markup("plan.runs_on_server_port_value2_e42376", {value2: String(release.port)})}</span>` : '');
    const progress = () => {
      const measured = release
        ? window.DevCoordinatorI18n.t(release.loc_total ? 'plan.linesDone' : 'plan.tasksDone', {done:locN(release.loc_total ? release.loc_done : release.tasks_done), total:locN(release.loc_total || release.tasks_total)})
        : window.DevCoordinatorI18n.t('plan.lineCount', {count:group.end - group.start, value:locN(group.end - group.start)});
      return measured + (group.unsizedCount ? ' · ' + window.DevCoordinatorI18n.t('plan.unestimatedCount', {count:group.unsizedCount}) : '');
    };
    const releaseBar = total && group.end > group.start
      ? `<div class="grelbar ${release?.status === 'delivered' ? 'delivered' : ''}" style="left:${pctOf(group.start)};width:${pctOf(group.end - group.start)}"><span>${(release ? esc(release.name) : window.DevCoordinatorI18n.markup("plan.not_scheduled_yet_6e4794"))}</span></div>`
      : '';
    return `<div class="grow grel"${droppable ? ` data-drop-release="${release ? esc(release.release_id) : ''}"` : ''}>
      <div class="glabel">
        <div class="plan-release-copy"><strong title="${esc(release ? release.name : window.DevCoordinatorI18n.t('plan.not_scheduled_yet_6e4794'))}" ${window.DevCoordinatorI18n.computedAttribute("title", () => release ? release.name : window.DevCoordinatorI18n.t("plan.not_scheduled_yet_6e4794"))}>${(release ? esc(release.name) : window.DevCoordinatorI18n.markup("plan.not_scheduled_yet_6e4794"))}</strong><span class="plan-release-meta">${release ? planBadge(release.status) : ''}${release?.kind === 'preview' && release.status !== 'requested' ? ` ${badge('preview')}` : ''}<span class="muted" title="${esc(progress())}" ${window.DevCoordinatorI18n.computedAttribute("title", progress)}>${window.DevCoordinatorI18n.computedMarkup(progress)}</span></span></div>
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
      ? `<button class="plan-tree-toggle" type="button" data-collapse="${esc(task.task_id)}" data-ui-continuation-anchor aria-expanded="${!row.collapsed}" aria-label="${esc(window.DevCoordinatorI18n.t(row.collapsed ? 'common.showSubtasks' : 'common.hideSubtasks', {name:task.title}))}" title="${esc(window.DevCoordinatorI18n.t(row.collapsed ? 'common.show_subtasks_fa0ff9' : 'common.hide_subtasks_07f203'))}" data-i18n-attrs='${esc(JSON.stringify({"aria-label":row.collapsed ? 'common.showSubtasks' : 'common.hideSubtasks',title:row.collapsed ? 'common.show_subtasks_fa0ff9' : 'common.hide_subtasks_07f203'}))}' data-i18n-args='${esc(JSON.stringify({name:task.title}))}'>${planIcon(row.collapsed ? 'chevron-right' : 'chevron-down')}</button>`
      : '<span class="plan-tree-toggle-spacer" aria-hidden="true"></span>';
    const drag = movable
      ? `<span class="plan-drag-handle" draggable="true" data-drag-task="${esc(task.task_id)}" title="Drag to move task" aria-label="${esc(window.DevCoordinatorI18n.t('plan.dragMoveTask', {title:task.title}))}" data-i18n-attrs='{"title":"plan.drag_to_move_task_41b0d9","aria-label":"plan.dragMoveTask"}' data-i18n-args='${esc(JSON.stringify({title:task.title}))}'>${planIcon('grip-vertical')}</span>`
      : '<span class="plan-drag-spacer" aria-hidden="true"></span>';
    const metaLoc = () => planEstimateText(row);
    const common = `role="button" tabindex="0" data-select-task="${esc(task.task_id)}" aria-pressed="${selected}" aria-label="${esc(window.DevCoordinatorI18n.t('plan.selectTask', {title:task.title}))}" data-i18n-attrs='{"aria-label":"plan.selectTask"}' data-i18n-args='${esc(JSON.stringify({title:task.title}))}'`;
    const hover = `data-hover-task="${esc(task.task_id)}" data-hover-title="${esc(task.title)}" data-hover-status="${esc(task.status)}" data-hover-loc="${esc(metaLoc())}" data-hover-progress="${esc(row.isUnsized ? window.DevCoordinatorI18n.t('plan.progressNeedsEstimate') : planDoneText(progress))}" data-hover-elaboration="${task.elaboration_needed ? 'true' : 'false'}"`;
    const sizedBar = row.width ? (row.isParent
      ? `<div class="gbar parent${selected ? ' selected' : ''}" style="left:${pctOf(row.start)};width:${pctOf(row.width)}" ${common}><span class="gdone" style="width:${progress.percent}%"></span></div>`
      : `<div class="gbar ${esc(task.status)}${selected ? ' selected' : ''}" style="left:${pctOf(row.start)};width:${pctOf(row.width)}" ${common} ${hover}>
          <span class="gdone" style="width:${progress.percent}%"></span><span class="gbar-label">${esc(task.title)}</span>
          ${pointerResizable ? `<button type="button" class="gresize" data-resize-handle="${esc(task.task_id)}" aria-label="${esc(window.DevCoordinatorI18n.t('plan.dragResizeTask', {title:task.title}))}" title="Drag to resize estimate" data-i18n-attrs='{"title":"plan.drag_to_resize_estimate_2d023d","aria-label":"plan.dragResizeTask"}' data-i18n-args='${esc(JSON.stringify({title:task.title}))}'></button>` : ''}
        </div>`) : '';
    const unsizedBar = row.unsizedCount ? `<div class="gbar unsized ${row.isParent ? 'parent ' : ''}${esc(task.status)}${selected ? ' selected' : ''}" style="left:${unsizedLeft};width:${unsizedWidth}" ${common} ${hover}><span class="gbar-label">${row.isParent ? window.DevCoordinatorI18n.markup('plan.unestimatedCount', {count:row.unsizedCount}) : esc(task.title)}</span></div>` : '';
    const bar = `${sizedBar}${unsizedBar}`;
    return `<div class="grow gtask${row.hidden ? ' ghidden' : ''}${selected ? ' selected' : ''}" data-task-row="${esc(task.task_id)}">
      <div class="glabel" style="--task-depth:${row.depth}">${drag}${collapse}<button type="button" class="plan-task-select" data-select-task="${esc(task.task_id)}" aria-pressed="${selected}">
        <span class="plan-task-title">${esc(task.title)}</span><span class="plan-task-meta" ${window.DevCoordinatorI18n.computedAttribute("title", () => `${metaLoc()} · ${window.DevCoordinatorI18n.t("common.status_" + task.status)}`)}><span title="${esc(metaLoc())}" ${window.DevCoordinatorI18n.computedAttribute("title", metaLoc)}>${window.DevCoordinatorI18n.computedMarkup(metaLoc)}</span>${planBadge(task.status)}${task.kind === 'user_feedback' ? ` ${badge('your request')}` : ''}</span>
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
  const axisTicks = total ? ticks.map((tick) => `<span style="left:${tick.left}">${window.DevCoordinatorI18n.computedMarkup(() => locN(tick.value))}</span>`).join('') : '';
  const unsizedAxis = unsizedCount ? `<span class="plan-axis-unsized" style="left:${unsizedLeft};width:${unsizedWidth}"><span data-i18n="plan.not_estimated_dbbcf5">Not estimated</span></span>` : '';
  const navigatorIcon = state.planNavigatorCollapsed ? 'layout-sidebar-left-expand' : 'layout-sidebar-left-collapse';
  const toolbar = `<div class="plan-toolbar" role="toolbar" aria-label="Timeline controls" data-i18n-attrs='{"aria-label":"plan.timeline_controls_f8f6dc"}'>
    <button class="plan-tool${state.planMode === 'select' ? ' active' : ''}" type="button" data-plan-mode="select" aria-pressed="${state.planMode === 'select'}" title="Select tasks" data-i18n-attrs='{"title":"plan.select_tasks_96e36b"}'>${planIcon('pointer')}<span class="sr-only"><span data-i18n="plan.select_tasks_96e36b">Select tasks</span></span></button>
    <button class="plan-tool${state.planMode === 'pan' ? ' active' : ''}" type="button" data-plan-mode="pan" aria-pressed="${state.planMode === 'pan'}" title="Pan timeline" data-i18n-attrs='{"title":"plan.pan_timeline_881955"}'>${planIcon('hand-stop')}<span class="sr-only"><span data-i18n="plan.pan_timeline_881955">Pan timeline</span></span></button>
    <span class="plan-tool-group" aria-label="Zoom controls" data-i18n-attrs='{"aria-label":"plan.zoom_controls_6af7d7"}'><button class="plan-tool" type="button" data-plan-zoom="out" title="Zoom out" data-i18n-attrs='{"title":"plan.zoom_out_bc7b63"}'>${planIcon('minus')}<span class="sr-only"><span data-i18n="plan.zoom_out_bc7b63">Zoom out</span></span></button><output id="plan-zoom-value" aria-live="polite">${state.planFit ? window.DevCoordinatorI18n.markup("plan.fit_9f872e") : `${Math.round(state.planZoom * 100)}%`}</output><button class="plan-tool" type="button" data-plan-zoom="in" title="Zoom in" data-i18n-attrs='{"title":"plan.zoom_in_0e47f0"}'>${planIcon('plus')}<span class="sr-only"><span data-i18n="plan.zoom_in_0e47f0">Zoom in</span></span></button></span>
    <button class="plan-tool" type="button" data-plan-zoom="fit" title="Fit the whole timeline" data-i18n-attrs='{"title":"plan.fit_the_whole_timeline_9cdb1c"}'>${planIcon('focus-centered')}<span class="sr-only"><span data-i18n="plan.fit_timeline_ff3a42">Fit timeline</span></span></button>
    <button class="plan-tool plan-nav-tool" type="button" data-plan-nav-toggle data-ui-continuation-anchor aria-pressed="${state.planNavigatorCollapsed}" title="${esc(window.DevCoordinatorI18n.t(state.planNavigatorCollapsed ? 'common.showTaskNavigator' : 'common.hideTaskNavigator'))}" data-i18n-attrs='${esc(JSON.stringify({title:state.planNavigatorCollapsed ? 'common.showTaskNavigator' : 'common.hideTaskNavigator'}))}'>${planIcon(navigatorIcon)}<span class="sr-only">${window.DevCoordinatorI18n.markup(state.planNavigatorCollapsed ? "common.showTaskNavigator" : "common.hideTaskNavigator")}</span></button>
  </div>`;
  const chartWidth = planCanvasWidth(total);
  const gantt = `<div class="plan-workspace${state.planNavigatorCollapsed ? ' navigator-collapsed' : ''}" style="--glabel:${state.planNavigatorCollapsed ? 0 : state.planNavigatorWidth}px;--chart-width:${chartWidth}px" data-ui-region="plan-primary">
    <div class="gantt-viewport${state.planMode === 'pan' ? ' pan-mode' : ''}" data-plan-viewport data-ui-allow-overlap="The frozen task navigator intentionally overlays timeline content that has scrolled behind it inside this bounded canvas" tabindex="0" aria-label="Interactive plan timeline. Scroll horizontally or use the timeline controls to pan and zoom." data-i18n-attrs='{"aria-label":"plan.interactive_plan_timeline_scroll_horizontally_or_f59ba1"}'>
      <div class="gantt" role="treegrid" aria-label="${esc(model.display_name)} task plan">
        <div class="gbounds">${gridLines}</div>
        <div class="grow gtools"><div class="glabel"><span class="plan-column-title"><span data-i18n="plan.tasks_b3a60e">Tasks</span></span><span class="plan-column-estimate"><span data-i18n="plan.est_lines_642941">Est. lines</span></span><button type="button" class="plan-nav-resizer" data-plan-nav-resizer role="separator" aria-orientation="vertical" aria-valuemin="260" aria-valuemax="520" aria-valuenow="${state.planNavigatorWidth}" aria-label="Resize task navigator" data-i18n-attrs='{"aria-label":"plan.resize_task_navigator_2cc8bc"}'></button></div><div class="gtrack">${toolbar}</div></div>
        <div class="grow gaxis"><div class="glabel muted"><span data-i18n="plan.plan_fa8ed0">Plan</span></div><div class="gtrack"><span class="plan-axis-title"><span data-i18n="plan.estimated_lines_of_code_f4fd5b">Estimated lines of code</span></span><div class="plan-axis-ticks">${axisTicks}${unsizedAxis}</div></div></div>
        ${groups.map((group) => releaseHead(group) + group.rows.map(taskRow).join('')).join('')}
      </div>
    </div>
    <div class="plan-minimap-row" aria-label="Timeline overview" data-i18n-attrs='{"aria-label":"plan.timeline_overview_4dc156"}'>
      <div class="plan-minimap-label"><span data-i18n="plan.timeline_overview_4dc156">Timeline overview</span></div>
      <div class="plan-minimap" data-plan-minimap role="scrollbar" tabindex="0" aria-label="Timeline horizontal position" aria-orientation="horizontal" aria-valuemin="0" aria-valuemax="100" aria-valuenow="0" data-i18n-attrs='{"aria-label":"plan.timeline_horizontal_position_81edc0"}'>
        <div class="plan-minimap-bars">${rows.filter((row) => !row.isParent && !row.hidden).map((row) => `<i data-minimap-task="${esc(row.task.task_id)}" class="${row.isUnsized ? 'unsized ' : ''}${esc(row.task.status)}${row.task.task_id === state.planSelectedTaskId ? ' selected' : ''}" style="left:${row.isUnsized ? unsizedLeft : pctOf(row.start)};width:${row.isUnsized ? unsizedWidth : pctOf(row.width)}"></i>`).join('')}</div>
        <div class="plan-minimap-thumb" data-plan-minimap-thumb></div>
      </div>
    </div>
  </div>`;

  const selectionTray = planSelectionTray(selectedRow, releaseById, admin);

  const contextHeader = `<section class="plan-context" data-ui-region="plan-context">
    <div class="plan-identity"><h1>${destinationLink('Plan', '#/plan')}</h1><span class="plan-slash" aria-hidden="true">/</span>${projectPicker(projects, repoId, (id) => `#/plan/${id}`, 'plan')}</div>
    <div class="plan-total-progress"><div><strong>${window.DevCoordinatorI18n.computedMarkup(() => locN(doneLoc))}</strong><span> <span data-i18n="plan.lines_done_e590ac">lines done</span></span></div><progress max="${Math.max(total, 1)}" value="${doneLoc}"></progress><div><strong>${window.DevCoordinatorI18n.computedMarkup(() => locN(total))}</strong><span> <span data-i18n="plan.lines_planned_38bf57">lines planned</span></span>${unsizedCount ? `<small>${window.DevCoordinatorI18n.markup("plan.unestimatedJobs", {count:unsizedCount})}</small>` : ''}</div></div>
    <div class="plan-context-actions"><a href="#/decisions/${esc(repoId)}"><span data-i18n="plan.decisions_7c5ca9">Decisions →</span></a>${admin ? `<button class="btn" type="button" data-plan-feedback>${planIcon('message-plus')}Ask for a change</button>` : ''}${requestControl}</div>
  </section>`;

  main.innerHTML = `${contextHeader}${requestNotice}${elaborationNotice}
    ${!model.releases.length && !model.tasks.length ? `<div data-ui-region="plan-primary">${stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_plan_yet_the_agent_will_publish_tasks_and_rel_52af00"))}</div>` : gantt}
    ${model.tasks_truncated ? "<p class=\"plan-footnote muted\"><span data-i18n=\"plan.only_the_newest_finished_tasks_are_shown_everyth_e411bf\">Only the newest finished tasks are shown; everything stays permanently recorded.</span></p>" : ''}
    ${selectionTray}
    <div class="plan-tooltip" id="plan-tooltip" role="tooltip" hidden></div>`;
  bind(main);
  bindProjectPicker(main);
  bindMoveButtons(main, model);
  bindPlanWorkspace(main, model, { groups, rows, total, unsizedCount, sizedShare, releaseById, admin, explicitVisibleTaskIds: state.planExplicitVisibleTaskIds });
  $('[data-plan-feedback]', main)?.addEventListener('click', () => openFeedbackDialog(repoId));
  if (admin) bindGanttDrag(main, model);
});

function openFeedbackDialog(repoId) {
  document.getElementById('feedback-dialog')?.remove();
  const returnFocus = document.activeElement;
  const dlg = document.createElement('dialog');
  dlg.id = 'feedback-dialog';
  dlg.innerHTML = `<div class="dialog-head"><div><h2><span data-i18n="plan.ask_for_a_change_f445fc">Ask for a change</span></h2><p class="muted"><span data-i18n="plan.your_request_becomes_a_task_in_this_plan_f67841">Your request becomes a task in this plan.</span></p></div><button class="dialog-close" type="button" data-dialog-close aria-label="Close" data-i18n-attrs='{"aria-label":"plan.close_7d9eb7"}'>${planIcon('x')}</button></div>
    <form id="comment-form" class="dialog-form">
      <label class="f"><span data-i18n="plan.what_you_want_4cc94b">What you want</span><input name="title" required maxlength="120" placeholder="The export button gives an error" data-i18n-attrs='{"placeholder":"plan.the_export_button_gives_an_error_39250f"}'></label>
      <label class="f"><span data-i18n="plan.why_it_matters_optional_4061bc">Why it matters (optional)</span><textarea name="impact"></textarea></label>
      <div class="dialog-actions"><button class="btn btn-primary" type="submit"><span data-i18n="plan.send_to_the_agent_d147f6">Send to the agent</span></button><button class="btn" type="button" data-dialog-close><span data-i18n="plan.cancel_19766e">Cancel</span></button></div>
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
  dlg.innerHTML = `<div class="dialog-head"><div><h2>${task.estimated_loc == null ? window.DevCoordinatorI18n.markup("plan.add_estimate_c87fb7") : window.DevCoordinatorI18n.markup("plan.change_estimate_6ae15e")}</h2><p class="muted">${esc(task.title)}</p></div><button class="dialog-close" type="button" data-dialog-close aria-label="Close" data-i18n-attrs='{"aria-label":"plan.close_7d9eb7"}'>${planIcon('x')}</button></div>
    <form id="estimate-form" class="dialog-form">
      <label class="f"><span data-i18n="plan.estimated_code_and_test_lines_5b58d6">Estimated code and test lines</span><input name="estimated_loc" type="number" inputmode="numeric" min="1" max="1000000" step="1" required value="${task.estimated_loc == null ? '' : Number(task.estimated_loc)}" placeholder="Enter a line estimate" data-i18n-attrs='{"placeholder":"plan.enter_a_line_estimate_7d4502"}'></label>
      <p class="muted"><span data-i18n="plan.include_test_code_and_fixtures_do_not_turn_test__6da1bb">Include test code and fixtures. Do not turn test-running or investigation time into pretend lines; that effort needs a separate hours estimate.</span></p>
      <div class="dialog-actions"><button class="btn btn-primary" type="submit"><span data-i18n="plan.save_estimate_5286c4">Save estimate</span></button><button class="btn" type="button" data-dialog-close><span data-i18n="plan.cancel_19766e">Cancel</span></button></div>
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
    button.disabled = false;
    button.setAttribute('aria-disabled', 'true');
    window.DevCoordinatorI18n.text(button, 'plan.explanationRequested', {title:task.title}, 'aria-label');
    window.DevCoordinatorI18n.text(button, 'plan.explanationRequested', {title:task.title}, 'title');
    button.innerHTML = `${planIcon('message-plus')}<span class="plan-elaborate-label"><span data-i18n="plan.requested_2d9e28">Requested</span></span>`;
  });
  root.querySelectorAll(`[data-hover-task="${CSS.escape(task.task_id)}"]`).forEach((bar) => {
    bar.dataset.hoverElaboration = 'true';
  });
  const notice = $('[data-plan-elaboration-notice]', root);
  if (notice) {
    const count = model.elaboration_requests.length;
    notice.hidden = false;
    window.DevCoordinatorI18n.bind(notice, () => window.DevCoordinatorI18n.t('common.tasksNeedExplanation', { count }));
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
        toast(() => window.DevCoordinatorI18n.t("common.could_not_request_elaboration_value1_b910db", {value1: error.message}), 'bad');
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
    window.DevCoordinatorI18n.bind(button, () => window.DevCoordinatorI18n.t(state.planSelectionCollapsed ? 'common.expandTaskDetails' : 'common.collapseTaskDetails'), "aria-label");
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
      let parent = groupRows.get(row.task.parent_task_id); let hidden = !layout.explicitVisibleTaskIds.has(row.task.task_id) && (row.depth === 0 || !row.isParent && row.depth > 0) && row.task.status === 'done';
      while (parent) {
        if (state.collapsed.has(parent.task.task_id)) { hidden = true; break; }
        parent = groupRows.get(parent.task.parent_task_id);
      }
      row.hidden = hidden;
      root.querySelector(`[data-task-row="${CSS.escape(row.task.task_id)}"]`)?.classList.toggle('ghidden', hidden);
      root.querySelector(`[data-minimap-task="${CSS.escape(row.task.task_id)}"]`)?.toggleAttribute('hidden', hidden);
      if (!row.isParent) continue;
      row.collapsed = state.collapsed.has(row.task.task_id);
      const button = root.querySelector(`[data-collapse="${CSS.escape(row.task.task_id)}"]`);
      if (!button) continue;
      const collapsed = state.collapsed.has(row.task.task_id);
      button.setAttribute('aria-expanded', String(!collapsed));
      window.DevCoordinatorI18n.bind(button, () => window.DevCoordinatorI18n.t(collapsed ? 'common.showSubtasks' : 'common.hideSubtasks', { name: row.task.title }), "aria-label");
      window.DevCoordinatorI18n.bind(button, () => (collapsed ? window.DevCoordinatorI18n.t("common.show_subtasks_fa0ff9") : window.DevCoordinatorI18n.t("common.hide_subtasks_07f203")), "title");
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
  let lastScrollTop = viewport.scrollTop;
  let alignFrame = 0;
  const alignTimelineToVisibleTask = () => {
    alignFrame = 0;
    const viewportBox = viewport.getBoundingClientRect();
    // Keep the first visible task bar just inside the timeline area below the
    // sticky controls. Hidden/collapsed rows are omitted from this search.
    const firstVisible = [...root.querySelectorAll('.gtask:not(.ghidden)')].find((element) => {
      const box = element.getBoundingClientRect();
      return box.bottom > viewportBox.top + 82 && box.top < viewportBox.bottom;
    });
    if (!firstVisible) return;
    const row = layout.rows.find((item) => item.task.task_id === firstVisible.dataset.taskRow);
    if (!row) return;
    const contentWidth = Math.max(1, currentChartWidth());
    const visibleWidth = visibleChartWidth();
    const maxScroll = Math.max(0, contentWidth - visibleWidth);
    const sizedShare = layout.sizedShare ?? 1;
    const hasUnsizedBar = row.isUnsized || (row.isParent && row.width === 0 && row.unsizedCount > 0);
    const chartOffset = hasUnsizedBar
      ? sizedShare * contentWidth
      : (total ? (row.start / total) * sizedShare * contentWidth : 0);
    const nextScroll = Math.max(0, Math.min(maxScroll, chartOffset));
    if (Math.abs(viewport.scrollLeft - nextScroll) < 1) return;
    viewport.scrollLeft = nextScroll;
    state.planScrollLeft = viewport.scrollLeft;
    updateMinimap();
  };
  const scheduleTimelineAlignment = () => {
    if (viewport.scrollTop === lastScrollTop) return;
    lastScrollTop = viewport.scrollTop;
    if (alignFrame) return;
    alignFrame = requestAnimationFrame(alignTimelineToVisibleTask);
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
  if (!state.planScrollLeft || state.planScrollTop) requestAnimationFrame(alignTimelineToVisibleTask);
  updateMinimap();
  viewport.addEventListener('scroll', () => {
    state.planScrollLeft = viewport.scrollLeft;
    state.planScrollTop = viewport.scrollTop;
    updateMinimap();
    scheduleTimelineAlignment();
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
    window.DevCoordinatorI18n.bind(button, () => window.DevCoordinatorI18n.t(state.planNavigatorCollapsed ? 'common.showTaskNavigator' : 'common.hideTaskNavigator'), "title");
    button.innerHTML = `${planIcon(state.planNavigatorCollapsed ? 'layout-sidebar-left-expand' : 'layout-sidebar-left-collapse')}<span class="sr-only">${window.DevCoordinatorI18n.markup(state.planNavigatorCollapsed ? "common.showTaskNavigator" : "common.hideTaskNavigator")}</span>`;
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
    const row = layout.rows.find(row => row.task.task_id === bar.dataset.hoverTask);
    if (row) {
      window.DevCoordinatorI18n.bind(bar, () => planEstimateText(row), 'data-hover-loc');
      window.DevCoordinatorI18n.bind(bar, () => row.isUnsized ? window.DevCoordinatorI18n.t('plan.progressNeedsEstimate') : planDoneText(planRowProgress(row)), 'data-hover-progress');
    }
    const show = () => {
      if (!tooltip) return;
      tooltip.innerHTML = `<strong>${esc(bar.dataset.hoverTitle)}</strong>${planBadge(bar.dataset.hoverStatus)}${bar.dataset.hoverElaboration === 'true' ? badge('elaboration needed', 'warn') : ''}<span>${window.DevCoordinatorI18n.computedMarkup(() => bar.dataset.hoverLoc)}</span><span>${window.DevCoordinatorI18n.computedMarkup(() => bar.dataset.hoverProgress)}</span>`;
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
        window.DevCoordinatorI18n.bind(readout, () => window.DevCoordinatorI18n.t("common.value1_lines_ba1a2e", {value1: locN(nextValue)}));
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
          toast(() => window.DevCoordinatorI18n.t("common.estimate_updated_to_value1_lines_d5443e", {value1: locN(nextValue)}), 'ok');
        } catch (error) { toast(() => window.DevCoordinatorI18n.t("common.resize_failed_value1_d279e7", {value1: error.message}), 'bad'); }
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
  dlg.innerHTML = `<h2>${window.DevCoordinatorI18n.markup("plan.move_value1_c26f36", {value1: task.title})}</h2>
    <p class="muted">Now in ${current ? esc(current.name) : window.DevCoordinatorI18n.markup("plan.not_scheduled_yet_333719")}.</p>
    <form id="move-form" class="inline">
      <label class="f"><span data-i18n="plan.move_to_ab270d">move to</span><select name="release_id">${open.map((r) => `<option value="${esc(r.release_id)}"${r.release_id === task.release_id ? ' selected' : ''}>${esc(r.name)}</option>`).join('')}<option value=""${task.release_id ? '' : ' selected'} data-i18n="plan.not_scheduled_yet_backlog_0f0d42">not scheduled yet (backlog)</option></select></label>
      <label class="f"><span data-i18n="plan.put_first_79c815">put first</span><input type="checkbox" name="first"></label>
      <div class="actions" style="flex-basis:100%">
        <button class="btn" type="submit"><span data-i18n="plan.move_6ecc3d">Move</span></button>
        <button class="btn" type="button" id="move-cancel"><span data-i18n="plan.cancel_19766e">Cancel</span></button>
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
    try { await api('task.update', args); toast(() => window.DevCoordinatorI18n.t("common.task_moved_104cc4"), 'ok'); render(); }
    catch (e) { toast(() => window.DevCoordinatorI18n.t("common.move_failed_value1_8cf7be", {value1: e.message}), 'bad'); }
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
    const head = `<strong>${esc(d.title)}</strong> ${badge(d.aspect.replace('_', ' '))}${d.ref ? ` <span class="muted mono">${esc(d.ref)}</span>` : ''} <span class="muted">${window.DevCoordinatorI18n.computedMarkup(() => ago(d.created_at))}</span>`;
    if (d.superseded_by) return `<details class="decision superseded"><summary>${head} ${badge('superseded')}</summary>${paragraphs(d.body)}</details>`;
    return `<div class="decision">${head}${paragraphs(d.body)}</div>`;
  };
  const story = !searching && !state.decisionBefore
    ? `<details class="story"><summary><span data-i18n="decisions.the_story_so_far_c8041c">The story so far</span></summary>${result.summary ? paragraphs(result.summary.body) : "<p class=\"muted\"><span data-i18n=\"decisions.no_summary_yet_959097\">No summary yet.</span></p>"}</details>` : '';
  const emptyText = searching ? 'Nothing found for that search.'
    : state.decisionAspect !== 'all' ? `No ${state.decisionAspect.replace('_', ' ')} decisions yet.`
      : 'No decisions yet. The agent records its choices here as it works.';
  main.innerHTML = `<div class="repository-context"><h1>${destinationLink('Decisions', '#/decisions')}</h1><span class="context-slash" aria-hidden="true">/</span>${projectPicker(projects, repoId, (id) => `#/decisions/${id}`, 'decisions')}</div>
    ${story}
    <form class="inline" id="decision-search"><label class="f"><span data-i18n="decisions.search_every_decision_6cd538">search every decision</span><input name="q" value="${esc(state.decisionQuery)}" placeholder="e.g. why exports are files" data-i18n-attrs='{"placeholder":"decisions.e_g_why_exports_are_files_e163cf"}'></label><button class="btn" type="submit"><span data-i18n="decisions.search_49c266">Search</span></button>${searching || state.decisionBefore ? "<button class=\"btn\" type=\"button\" id=\"decisions-latest\"><span data-i18n=\"decisions.show_latest_3bf9ac\">Show latest</span></button>" : ''}</form>
    <div class="segwrap">${seg(ASPECTS, state.decisionAspect, 'decision-aspect', (o) => o.replace('_', ' '))}</div>
    ${entries.length ? entries.map(card).join('') : stateBlock('empty', emptyText)}
    ${!searching && result.has_more ? "<p><button class=\"btn\" id=\"decisions-older\"><span data-i18n=\"decisions.show_older_decisions_7fa06a\">Show older decisions</span></button></p>" : ''}`;
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
const languageReady = import("./i18n.mjs").then(({ i18n }) => i18n.ready);
let renderRequest = 0;
const performancePage = window.DevCoordinatorPerformance.create({ api, esc, compactNumber, durationMs, coverageText, identity: () => state.who });
const workspace = window.DevCoordinatorWorkspace.create({ api, esc, identity: () => state.who });
window.addEventListener('resize', () => {
  const composer = $('#evidence-composer', main);
  if (!composer || composer.hidden) return;
  openEvidenceComposer(undefined, false);
});
async function render() {
  const request = ++renderRequest;
  const requestedHash = location.hash;
  await languageReady;
  if (request !== renderRequest || requestedHash !== location.hash) return;
  await window.DevCoordinatorI18n.route();
  // A newer navigation may still be loading its own message catalog. Do not
  // resolve that newer route using the catalog loaded for this older request.
  if (request !== renderRequest || requestedHash !== location.hash) return;
  closeGlossaryDialog?.();
  closeActiveProjectPicker?.(false);
  document.body.classList.remove('sketch-drawer-open');
  viewAbort?.abort();
  viewAbort = new AbortController();
  const signal = viewAbort.signal;
  let route;
  try { route = await workspace.resolve(signal); }
  catch (error) {
    if (request !== renderRequest || signal.aborted || error.code === 'stale' || error.code === 'unauthenticated') return;
    if (['test_evidence_expired', 'test_evidence_not_found'].includes(error.code)) {
      main.innerHTML = currentDestinationHeading() + stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_visual_evidence_is_available_for_this_run_it__4f1873"));
      return;
    }
    main.innerHTML = currentDestinationHeading() + stateBlock(error.code === 'permission_denied' ? 'denied' : 'error', () => error.message);
    return;
  }
  if (!route || signal.aborted || request !== renderRequest) return;
  const { view, arg } = route;
  main.classList.toggle('plan-page', view === 'plan' && !!arg);
  main.classList.toggle('glossary-page', view === 'glossary');
  main.classList.toggle('usage-page', view === 'usage' && !!arg);
  main.classList.toggle('progress-page', view === 'progress' && !!arg);
  main.classList.toggle('performance-page', view === 'performance');
  main.classList.toggle('health-page', view === 'health');
  main.classList.toggle('tickets-page', view === 'requests');
  if (view !== 'requests') main.classList.remove('ticket-selected');
  main.classList.toggle('deployments-page', view === 'deployments' && !arg);
  const sketchQuery = new URLSearchParams(location.hash.split('?')[1] || '');
  const sketchDetailRoute = view === 'sketches' && sketchQuery.has('sketch') && !sketchQuery.has('set') && (!sketchQuery.has('surface') || sketchQuery.has('annotate'));
  const sketchSetEvidenceRoute = view === 'sketches' && sketchQuery.has('sketch') && sketchQuery.has('set');
  main.classList.toggle('test-evidence-page', (view === 'tests' && !!arg) || sketchDetailRoute || sketchSetEvidenceRoute);
  main.classList.toggle('tests-collection-page', view === 'tests' && !arg);
  main.classList.toggle('sketches-page', view === 'sketches');
  main.classList.toggle('sketch-history-page', view === 'sketches' && !sketchQuery.has('annotate') && (sketchQuery.has('surface') || !sketchQuery.has('sketch')));
  document.body.classList.toggle('plan-shell', view === 'plan' && !!arg);
  document.body.classList.toggle('evidence-shell', (view === 'tests' && !!arg) || sketchDetailRoute || sketchSetEvidenceRoute);
  if (!(view === 'tests' && arg) && !sketchDetailRoute && !sketchSetEvidenceRoute && state.evidenceRunId) {
    evidenceCanvasSession?.observer?.disconnect(); evidenceCanvasSession = null;
    resetEvidenceImages(); state.evidenceRunId = null; state.evidenceData = null;
    state.evidenceSteps = []; state.evidenceRun = null;
    state.evidenceSource = 'test'; state.sketchDetail = null;
  }
  document.querySelectorAll('#nav a').forEach((a) => a.classList.toggle('active', a.dataset.view === view));
  setBanner('');
  if (route.empty) {
    main.innerHTML = currentDestinationHeading() + stateBlock('empty', () => window.DevCoordinatorI18n.t("common.no_repositories_visible_to_you_40cf12"));
    return;
  }
  if (view === 'deployments') return arg ? viewDeployment(arg) : viewDeployments();
  if (view === 'plan' && new URLSearchParams(location.hash.split('?')[1] || '').has('task')) state.planRequestedTaskId = new URLSearchParams(location.hash.split('?')[1]).get('task');
  if (view === 'plan') return arg ? viewPlan(arg) : viewPlanPicker('plan');
  if (view === 'progress') return arg ? viewProgress(arg) : viewProgressRepositories();
  if (view === 'performance') return performancePage.render(main, arg, signal);
  if (view === 'usage') return arg ? viewCodexUsage(arg) : viewCodexUsageRepositories();
  if (view === 'decisions' && new URLSearchParams(location.hash.split('?')[1] || '').has('q')) state.decisionQuery = new URLSearchParams(location.hash.split('?')[1]).get('q');
  if (view === 'decisions') return arg ? viewDecisions(arg) : viewPlanPicker('decisions');
  if (view === 'glossary') return viewGlossary();
  if (view === 'tests') return viewTests(arg || null, route.settings);
  if (view === 'sketches') return viewSketches(arg, sketchQuery.get('sketch'), sketchQuery.get('set'));
  if (view === 'health') return viewHealth(arg);
  if (view === 'bugs') return viewBugs();
  if (view === 'requests') return window.DevCoordinatorTickets.mount(main, {api:(operation,params)=>api(operation,params,false),administrator:state.who?.administrator,signal});
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
  await languageReady;
  try {
    state.who = await api('user.whoami', {});
    window.DevCoordinatorI18n.bind($('#who-email'), () => (state.who.identity || window.DevCoordinatorI18n.t("common.local_25bf8e")));
    $('#nav-admin').hidden = !state.who.administrator;
  } catch (e) { if (e.code !== 'unauthenticated') setBanner(() => window.DevCoordinatorI18n.t("common.cannot_reach_the_coordinator_value1_7f685b", {value1: e.message})); }
  render();
})();
