import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';

const kinds = new Set(['hidden-navigation-track', 'primary-content-width', 'readable-heading', 'readable-canonical-identifier', 'no-character-wrapping', 'document-horizontal-overflow', 'initial-viewport-placement', 'clipping']);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const stable = value => JSON.stringify(value, (_key, item) => item && typeof item === 'object' && !Array.isArray(item) ? Object.fromEntries(Object.entries(item).sort(([a], [b]) => a.localeCompare(b))) : item);
const sha = value => digest(stable(value));
function text(value, label) {
  if (typeof value !== 'string' || !value.trim() || value.length > 2048) throw new Error(`${label} must be non-empty bounded text`);
  return value.trim();
}
function finite(value, label, minimum = 0) {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < minimum) throw new Error(`${label} must be finite and >= ${minimum}`);
  return value;
}
function list(value, label) {
  if (value === undefined) return [];
  if (!Array.isArray(value) || value.length > 256) throw new Error(`${label} must be a bounded array`);
  const ids = new Set();
  return value.map((row, index) => {
    if (!row || typeof row !== 'object' || Array.isArray(row)) throw new Error(`${label}[${index}] must be an object`);
    const id = text(row.id, `${label}[${index}].id`);
    if (ids.has(id)) throw new Error(`${label} contains duplicate id ${id}`);
    ids.add(id); return { ...row, id };
  });
}
function known(row, keys, label) {
  if (Object.keys(row).some(key => !keys.includes(key))) throw new Error(`${label} contains an unsupported field`);
}
export function normalizeGeometry(value, label = 'geometryAssertions') {
  return list(value, label).map(row => {
    known(row, ['id', 'kind', 'selector', 'region', 'minWidth', 'minWidthRatio', 'allowance', 'primarySelector', 'track', 'maxReservedSize'], label);
    if (!kinds.has(row.kind)) throw new Error(`${label} has unsupported kind`);
    if (Boolean(row.selector) === Boolean(row.region)) throw new Error(`${label} requires selector XOR region`);
    const result = { id: row.id, kind: row.kind, ...(row.selector ? { selector: text(row.selector, 'selector') } : { region: text(row.region, 'region') }) };
    for (const bound of ['minWidth', 'minWidthRatio']) if (row[bound] !== undefined) result[bound] = finite(row[bound], bound);
    if (row.kind === 'primary-content-width' && result.minWidth === undefined && result.minWidthRatio === undefined) throw new Error('primary-content-width requires an explicit width bound');
    if (row.allowance !== undefined) { known(row.allowance, ['reason'], 'allowance'); result.allowance = { reason: text(row.allowance?.reason, 'allowance.reason') }; }
    if (row.kind === 'hidden-navigation-track') {
      result.primarySelector = text(row.primarySelector, 'primarySelector');
      const track = row.track;
      if (!track || !['columns', 'rows'].includes(track.axis) || !Number.isInteger(track.index) || track.index < 0) throw new Error('hidden-navigation-track requires a grid axis and nonnegative track index');
      result.track = { selector: text(track.selector, 'track.selector'), axis: track.axis, index: track.index };
      known(track, ['selector', 'axis', 'index'], 'track');
      result.maxReservedSize = finite(row.maxReservedSize, 'maxReservedSize');
    }
    return result;
  });
}
export function normalizeShapes(value) {
  return list(value, 'fixtureDataShapes').map(row => {
    known(row, ['id', 'revision', 'target', 'route', 'state', 'conditionalDom', 'layoutEffect'], 'fixtureDataShapes');
    if (!Array.isArray(row.conditionalDom) || !row.conditionalDom.length || row.conditionalDom.length > 128) throw new Error('fixtureDataShapes requires bounded non-empty conditionalDom');
    const route = text(row.route, 'route');
    if (!route.startsWith('/') || route.includes('#')) throw new Error('shape route must be a path plus optional query');
    return { id: row.id, revision: text(row.revision, 'revision'), route, state: text(row.state, 'state'), ...(row.target !== undefined ? { target: text(row.target, 'target') } : {}), conditionalDom: row.conditionalDom.map(selector => text(selector, 'conditionalDom selector')), layoutEffect: text(row.layoutEffect, 'layoutEffect') };
  });
}
export function normalizeReported(value) {
  return list(value, 'reportedBrowserStates').map(row => {
    known(row, ['id', 'target', 'state', 'theme', 'viewport', 'device', 'userAgent', 'auth', 'zoom', 'engine'], 'reportedBrowserStates');
    const viewport = row.viewport;
    if (!viewport || !Number.isInteger(viewport.width) || !Number.isInteger(viewport.height) || viewport.width <= 0 || viewport.height <= 0) throw new Error('reportedBrowserStates viewport requires positive integer dimensions');
    known(viewport, ['name', 'width', 'height'], 'reported viewport');
    return { id: row.id, target: text(row.target, 'target'), state: text(row.state, 'state'), theme: text(row.theme, 'theme'), viewport: { name: text(viewport.name, 'viewport.name'), width: viewport.width, height: viewport.height }, device: text(row.device, 'device'), userAgent: text(row.userAgent, 'userAgent'), auth: text(row.auth, 'auth'), zoom: finite(row.zoom, 'zoom', Number.MIN_VALUE), ...(row.engine !== undefined ? { engine: text(row.engine, 'engine') } : {}) };
  });
}
function targetName(target) { return target.baseTargetName || target.name || target.url; }
function route(target) { const parsed = new URL(target.continuation?.kind === 'navigation' ? target.continuation.expectedPath : target.url, target.url); return parsed.pathname + parsed.search; }
export function resolveHandoffRequirements(config, cells) {
  const gaps = [];
  const shapes = config.fixtureDataShapes.map(shape => {
    const matching = cells.filter(cell => route(cell.target) === shape.route && cell.target.stateName === shape.state && (!shape.target || targetName(cell.target) === shape.target));
    const groups = new Set(matching.map(cell => cell.target.targetGroupId));
    const status = matching.length && groups.size === 1 ? 'mapped' : 'incomplete';
    if (status !== 'mapped') gaps.push({ id: shape.id, kind: 'data-shape', reason: matching.length ? 'ambiguous-target' : 'missing-cell' });
    return { id: shape.id, status, cells: status === 'mapped' ? matching.map(cell => cell.cellId) : [] };
  });
  const reported = config.reportedBrowserStates.map(row => {
    const matching = cells.filter(cell => targetName(cell.target) === row.target && cell.target.stateName === row.state && cell.target.theme === row.theme && cell.viewport.name === row.viewport.name && cell.viewport.width === row.viewport.width && cell.viewport.height === row.viewport.height);
    const required = config.requiredCoverage.filter(required => required.target === row.target && required.state === row.state && required.viewport === row.viewport.name && (required.width === null || required.width === undefined || required.width === row.viewport.width));
    const status = matching.length === 1 && required.length === 1 ? 'mapped' : 'incomplete';
    if (status !== 'mapped') gaps.push({ id: row.id, kind: 'reported-browser', reason: 'missing-or-ambiguous-required-cell' });
    return { id: row.id, status, cells: status === 'mapped' ? [matching[0].cellId] : [] };
  });
  return { shapes, reported, gaps };
}

// This function is serialized into the real page. It returns numbers, booleans
// and selectors only; inspected text and native control values never escape.
export function measureHandoffInPage(input) {
  const find = selector => { try { return [...document.querySelectorAll(selector)]; } catch { return []; } };
  const visible = element => Boolean(element && element.getClientRects().length && element.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true }));
  const rect = element => { const box = element.getBoundingClientRect(); return { x: box.x, y: box.y, width: box.width, height: box.height }; };
  const clips = element => {
    const box = element.getBoundingClientRect(), rows = [];
    if (element.clientWidth > 0 && element.scrollWidth > element.clientWidth + 1 && ['hidden', 'clip'].includes(getComputedStyle(element).overflowX)) rows.push({ axis: 'x', self: true });
    if (element.clientHeight > 0 && element.scrollHeight > element.clientHeight + 1 && ['hidden', 'clip'].includes(getComputedStyle(element).overflowY)) rows.push({ axis: 'y', self: true });
    for (let parent = element.parentElement; parent; parent = parent.parentElement) {
      const style = getComputedStyle(parent), bounds = parent.getBoundingClientRect();
      if (['hidden', 'clip'].includes(style.overflowX) && (box.left < bounds.left - 1 || box.right > bounds.right + 1)) rows.push({ axis: 'x', self: false });
      if (['hidden', 'clip'].includes(style.overflowY) && (box.top < bounds.top - 1 || box.bottom > bounds.bottom + 1)) rows.push({ axis: 'y', self: false });
    }
    return rows;
  };
  const wrapping = element => {
    const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT), lines = [];
    let node, count = 0;
    const segmenter = new Intl.Segmenter(undefined, { granularity: 'grapheme' });
    while ((node = walker.nextNode())) for (const segment of segmenter.segment(node.textContent || '')) {
      if (!segment.segment.trim()) continue;
      if (++count > 8192) return { status: 'incomplete', reason: 'grapheme-budget-exceeded' };
      const range = document.createRange(); range.setStart(node, segment.index); range.setEnd(node, segment.index + segment.segment.length);
      const boxes = [...range.getClientRects()].filter(box => box.width > 0 && box.height > 0);
      if (!boxes.length) continue;
      const top = boxes[0].top; let line = lines.find(line => Math.abs(line.top - top) <= 1);
      if (!line) { line = { top, count: 0 }; lines.push(line); } line.count++;
    }
    const measured = lines.reduce((sum, line) => sum + line.count, 0);
    return { status: 'measured', graphemeCount: measured, lineCount: lines.length, maximumLineGraphemes: Math.max(0, ...lines.map(line => line.count)), characterByCharacter: measured > 1 && lines.length > 1 && lines.every(line => line.count <= 1) };
  };
  const geometry = input.assertions.map(assertion => {
    const elements = find(assertion.selector), row = { id: assertion.id, kind: assertion.kind, selector: assertion.selector, status: 'incomplete', measurements: { matchCount: elements.length } };
    if (elements.length !== 1) { row.reason = 'missing-or-ambiguous-selector'; return row; }
    const element = elements[0], box = element.getBoundingClientRect(), shown = visible(element), clipped = clips(element);
    Object.assign(row.measurements, { visible: shown, rect: rect(element), clipped });
    let passed;
    if (assertion.kind === 'primary-content-width') {
      row.measurements.requiredWidth = Math.max(assertion.minWidth ?? 0, (assertion.minWidthRatio ?? 0) * innerWidth);
      if (!Number.isFinite(row.measurements.requiredWidth)) { row.reason = 'nonfinite-derived-width-bound'; return row; }
      passed = shown && box.width >= row.measurements.requiredWidth;
    } else if (assertion.kind === 'document-horizontal-overflow') {
      Object.assign(row.measurements, { scrollWidth: document.documentElement.scrollWidth, clientWidth: document.documentElement.clientWidth });
      passed = document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1;
    } else if (assertion.kind === 'initial-viewport-placement') {
      passed = shown && box.right > 0 && box.left < innerWidth && box.bottom > 0 && box.top < innerHeight;
    } else if (assertion.kind === 'hidden-navigation-track') {
      const tracks = find(assertion.track.selector), primary = find(assertion.primarySelector);
      Object.assign(row.measurements, { trackMatchCount: tracks.length, primaryMatchCount: primary.length });
      if (tracks.length !== 1 || primary.length !== 1) { row.reason = 'missing-or-ambiguous-track-or-primary'; return row; }
      const style = getComputedStyle(tracks[0]), value = assertion.track.axis === 'columns' ? style.gridTemplateColumns : style.gridTemplateRows;
      if (!['grid', 'inline-grid'].includes(style.display) || !/^(?:\d+(?:\.\d+)?px\s*)+$/.test(value)) { row.reason = 'unresolved-grid-track'; return row; }
      const sizes = value.trim().split(/\s+/).map(value => Number.parseFloat(value));
      const reserved = sizes[assertion.track.index];
      if (!Number.isFinite(reserved)) { row.reason = 'missing-grid-track'; return row; }
      const primaryAssertions = input.assertions.filter(candidate => candidate.kind === 'primary-content-width' && find(candidate.selector).length === 1 && find(candidate.selector)[0] === primary[0]);
      if (!primaryAssertions.length) { row.reason = 'missing-primary-width-assertion'; return row; }
      Object.assign(row.measurements, { reservedSize: reserved, maxReservedSize: assertion.maxReservedSize, primaryWidth: primary[0].getBoundingClientRect().width, primaryAssertionIds: primaryAssertions.map(assertion => assertion.id) });
      passed = !shown && reserved <= assertion.maxReservedSize;
    } else if (assertion.kind === 'no-character-wrapping') {
      const measured = wrapping(element); row.measurements.wrapping = measured;
      if (measured.status !== 'measured') { row.reason = measured.reason; return row; }
      passed = shown && !measured.characterByCharacter;
    } else if (assertion.kind === 'clipping') passed = shown && !clipped.length;
    else {
      const measured = wrapping(element); row.measurements.wrapping = measured;
      const nativeTextPresent = (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) && Boolean(element.value || element.placeholder);
      row.measurements.nativeControlTextPresent = nativeTextPresent;
      if (measured.status !== 'measured' || (!measured.graphemeCount && !nativeTextPresent)) { row.reason = 'rendered-text-unavailable'; return row; }
      passed = shown && box.width > 0 && box.height > 0 && !clipped.length;
    }
    if (assertion.minWidth !== undefined || assertion.minWidthRatio !== undefined) {
      row.measurements.requiredWidth = Math.max(assertion.minWidth ?? 0, (assertion.minWidthRatio ?? 0) * innerWidth);
      passed &&= box.width >= row.measurements.requiredWidth;
    }
    row.status = passed ? 'passed' : assertion.allowance ? 'allowed' : 'failed';
    if (!passed && assertion.allowance) row.appliedAllowance = assertion.allowance;
    return row;
  });
  for (const row of geometry.filter(row => row.kind === 'hidden-navigation-track' && ['passed', 'allowed'].includes(row.status))) {
    if (!row.measurements.primaryAssertionIds.every(id => geometry.some(assertion => assertion.id === id && ['passed', 'allowed'].includes(assertion.status)))) { row.status = 'failed'; row.reason = 'primary-width-failed'; }
  }
  const shapes = input.shapes.map(shape => ({ id: shape.id, revision: shape.revision, conditionalDom: shape.conditionalDom.map(selector => {
    const elements = find(selector); return { selector, attachedCount: elements.length, visibleCount: elements.filter(visible).length };
  }) })).map(shape => ({ ...shape, status: shape.conditionalDom.every(row => row.attachedCount > 0) ? 'passed' : 'incomplete' }));
  return { geometry, shapes, browser: { innerWidth, innerHeight, devicePixelRatio, userAgent: navigator.userAgent, touchPoints: navigator.maxTouchPoints, visualViewportScale: window.visualViewport?.scale ?? null, cssZoom: getComputedStyle(document.documentElement).zoom } };
}

export async function measureHandoff(page, target, viewport, config, cellId) {
  const gaps = [];
  const assertions = (target.geometryAssertions || []).map(assertion => {
    if (assertion.selector) return assertion;
    const regions = target.regions.filter(region => region.name === assertion.region);
    if (regions.length !== 1) gaps.push({ id: assertion.id, reason: 'missing-or-ambiguous-region' });
    return { ...assertion, selector: regions.length === 1 ? regions[0].selector : ':not(*)' };
  });
  if (!assertions.length) gaps.push({ reason: 'geometry-not-declared' });
  const mappedShapes = new Set((config.handoffRequirements?.shapes || []).filter(row => row.cells.includes(cellId)).map(row => row.id));
  const shapes = config.fixtureDataShapes.filter(shape => mappedShapes.has(shape.id));
  if (!shapes.length) gaps.push({ reason: 'data-shape-not-declared' });
  const measured = await page.evaluate(measureHandoffInPage, { assertions, shapes });
  const observedRoute = new URL(page.url());
  for (const row of measured.shapes) {
    row.routeMatched = shapes.find(shape => shape.id === row.id).route === observedRoute.pathname + observedRoute.search;
    if (!row.routeMatched) { row.status = 'incomplete'; row.reason = 'rendered-route-query-mismatch'; }
  }
  const browser = page.context().browser();
  measured.browser.engine = browser?.browserType().name() ?? null;
  measured.browser.version = browser?.version() ?? null;
  measured.browser.device = viewport.device || 'desktop';
  measured.browser.emulatedDevice = Boolean(viewport.device);
  measured.browser.isMobile = Boolean(viewport.contextOptions?.isMobile);
  measured.browser.auth = target.authProfile || (config.cookies.length ? 'unprofiled' : 'anonymous');
  measured.browser.browserZoom = { value: 1, guarantee: 'fresh-isolated-context-default-browser-zoom' };
  const mappedReported = new Set((config.handoffRequirements?.reported || []).filter(row => row.cells.includes(cellId)).map(row => row.id));
  measured.reported = config.reportedBrowserStates.filter(row => mappedReported.has(row.id)).map(row => ({
    id: row.id,
    status: row.zoom !== 1 ? 'incomplete' : ((!row.engine || row.engine === measured.browser.engine) && row.device === measured.browser.device && row.userAgent === measured.browser.userAgent && row.auth === measured.browser.auth && row.viewport.width === measured.browser.innerWidth && row.viewport.height === measured.browser.innerHeight ? 'passed' : 'incomplete'),
    reason: row.zoom !== 1 ? 'nondefault-browser-zoom-unmeasured' : null,
  }));
  measured.gaps = gaps;
  const findings = measured.geometry.filter(row => ['failed', 'allowed'].includes(row.status)).map(row => ({ severity: row.status === 'failed' ? 'critical' : 'warning', rule: row.status === 'failed' ? `geometry-${row.kind}` : 'allowed-geometry-assertion', selector: row.selector, message: row.status === 'failed' ? 'Declared geometry assertion failed.' : 'Measured geometry difference has an explicit allowance.', textSnippet: '', rect: row.measurements.rect, area: null, evidence: row }));
  return { measured, findings };
}

function screenshotFiles(report) {
  return report.pages.flatMap(page => [page.screenshots?.viewport, page.screenshots?.fullPage]).filter(Boolean);
}
export function formalDecision(report, exitCode, config, blocking) {
  const gaps = [...(config.handoffRequirements?.gaps || [])];
  for (const page of report.pages) {
    const evidence = page.metrics?.handoff;
    if (!evidence) gaps.push({ cellId: page.cellId, reason: 'handoff-measurements-missing' });
    else {
      gaps.push(...evidence.gaps.map(gap => ({ cellId: page.cellId, ...gap })));
      for (const kind of ['primary-content-width', 'readable-heading', 'readable-canonical-identifier', 'no-character-wrapping', 'document-horizontal-overflow', 'initial-viewport-placement', 'clipping']) if (!evidence.geometry.some(row => row.kind === kind)) gaps.push({ cellId: page.cellId, reason: `required-geometry-${kind}-missing` });
      for (const row of [...evidence.geometry, ...evidence.shapes, ...evidence.reported]) if (row.status === 'incomplete') gaps.push({ cellId: page.cellId, id: row.id, reason: row.reason || 'missing-measurement' });
    }
    if (page.sourceBinding?.status !== 'matched' || !page.review?.sourceFingerprint) gaps.push({ cellId: page.cellId, reason: 'source-identity-unbound' });
    if (!page.metrics?.performance?.ttfb || !page.metrics?.performance?.lcp || Object.values(page.metrics?.performance || {}).some(metric => metric?.status === 'unavailable')) gaps.push({ cellId: page.cellId, reason: 'required-performance-unavailable' });
  }
  const coverage = report.coverage || {};
  const freshComplete = Boolean(coverage.readinessEligible && coverage.coverageMode === 'complete' && !coverage.failed && report.pages.length > 0 && report.pages.length === coverage.fullDeclaredPages && report.pages.every(page => page.outcome === 'checked' && !page.cache?.hit));
  const unavailable = report.execution?.unsafeStop === 'browser-authority-lost' || report.pages.some(page => ['auth_setup_error', 'evidence_error', 'navigation_failed', 'navigation_error', 'http_error', 'non_html', 'browser_authority_lost', 'internal_cell_error'].includes(page.outcome) || page.evidenceErrors?.length || (page.outcome === 'checked' && (!page.screenshots?.viewport || !page.screenshots?.fullPage)));
  const renderedFailure = blocking.length > 0 || report.pages.some(page => page.outcome === 'interaction_error' || page.interactionFailure || page.metrics?.handoff?.geometry.some(row => row.status === 'failed'));
  const result = exitCode === 2 || unavailable ? 'blocked' : renderedFailure ? 'failed' : freshComplete && !gaps.length && exitCode === 0 ? 'passed' : 'incomplete';
  const sourceInputs = report.pages.map(page => ({ cellId: page.cellId, inputFingerprint: page.review?.sourceFingerprint || null, observedBindingSha256: page.sourceBinding?.status === 'matched' ? sha(page.sourceBinding.observed) : null }));
  const sourceSha256 = sourceInputs.length && sourceInputs.every(row => row.inputFingerprint && row.observedBindingSha256) ? sha(sourceInputs) : null;
  const planSha256 = sha(report.plan || {});
  const configSha256 = report.evidence?.config?.sha256 || null, verifierSha256 = report.evidence?.verifier?.sha256 || null;
  const candidateId = sourceSha256 && configSha256 && verifierSha256 ? sha({ sourceSha256, configSha256, verifierSha256, planSha256 }) : null;
  return { result, runId: report.runId, candidateId, exitCode, freshComplete, sourceSha256, sourceDigestScope: 'declared UI input fingerprints and matched observed source bindings; not the complete repository', configSha256, verifierSha256, planSha256, coverage: { status: !gaps.length && freshComplete ? 'passed' : 'incomplete', readinessEligible: Boolean(coverage.readinessEligible), requiredCells: coverage.fullDeclaredPages ?? report.pages.length, checkedCells: coverage.checkedPages ?? 0, gapCount: gaps.length }, evidence: { report: 'report.json', journeyEvidence: 'journey-evidence.json', reviewQueue: 'review-queue.json', screenshots: 'screenshots/', artifactManifest: 'formal-artifacts.json', manifestSha256: null }, gaps };
}

export function retainFormalReceipt(report, artifacts) {
  const files = [artifacts.jsonOut, artifacts.markdownOut, artifacts.reviewQueueOut, artifacts.journeyEvidenceOut, ...screenshotFiles(report).map(row => row.path)];
  const unique = [...new Set(files)], manifest = { schemaVersion: 1, kind: 'formal-ui-artifact-manifest', runId: report.runId, files: [] };
  const directory = path.dirname(artifacts.jsonOut);
  const expected = new Map(screenshotFiles(report).map(row => [row.path, row.sha256]));
  expected.set(artifacts.reviewQueueOut, report.review?.queueSha256);
  expected.set(artifacts.journeyEvidenceOut, report.evidence?.journey?.sha256);
  for (const filename of unique) {
    if (!filename) throw new Error('Required formal artifact path is missing');
    const stat = fs.lstatSync(filename);
    if (!stat.isFile() || stat.isSymbolicLink()) throw new Error('Required formal artifact is unavailable');
    const bytes = fs.readFileSync(filename);
    if (expected.has(filename) && expected.get(filename) !== digest(bytes)) throw new Error('Retained formal artifact does not match its recorded identity');
    manifest.files.push({ identity: digest(filename), kind: filename === artifacts.jsonOut ? 'report' : filename === artifacts.markdownOut ? 'markdown' : filename === artifacts.reviewQueueOut ? 'review-queue' : filename === artifacts.journeyEvidenceOut ? 'journey-evidence' : 'screenshot', sha256: digest(bytes), bytes: bytes.length });
  }
  const bytes = Buffer.from(`${JSON.stringify(manifest)}\n`), manifestPath = path.join(directory, 'formal-artifacts.json');
  fs.writeFileSync(manifestPath, bytes, { mode: 0o600 });
  const formal = { ...report.formal, evidence: { ...report.formal.evidence, report: path.basename(artifacts.jsonOut), journeyEvidence: path.relative(directory, artifacts.journeyEvidenceOut), reviewQueue: path.relative(directory, artifacts.reviewQueueOut), screenshots: path.relative(directory, artifacts.screenshotDir), manifestSha256: digest(bytes) } };
  // Detailed gap rows remain in the hash-bound report, not the bounded receipt.
  delete formal.gaps;
  fs.writeFileSync(path.join(directory, 'formal-receipt.json'), `${JSON.stringify({ formal })}\n`, { mode: 0o600 });
  return formal;
}
