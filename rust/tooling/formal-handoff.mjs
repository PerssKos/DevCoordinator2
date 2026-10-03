import fs from 'node:fs';
import { createHash } from 'node:crypto';

export const requiredGeometryKinds = Object.freeze([
  'hidden-navigation-track', 'primary-content-width', 'readable-heading',
  'readable-canonical-identifier', 'no-character-wrapping',
  'document-horizontal-overflow', 'initial-viewport-placement', 'clipping',
]);

// Executed in the browser. Return geometry and stable IDs, never rendered text.
export function measureDeclaredLayout({ assertions = [], shapes = [] }) {
  const visible = element => {
    const style = getComputedStyle(element), rect = element.getBoundingClientRect();
    return style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0;
  };
  const measured = assertions.map(assertion => {
    const { id, kind, selector } = assertion;
    const result = { id, kind, selector, result: 'incomplete', measurements: {} };
    if (!id || !kind) return result;
    let elements;
    try { elements = kind === 'document-horizontal-overflow' ? [document.documentElement] : [...document.querySelectorAll(selector)]; }
    catch { return result; }
    if (!elements.length) return result;
    const observations = elements.map(element => {
      const rect = element.getBoundingClientRect(), style = getComputedStyle(element);
      const width = rect.width, height = rect.height, shown = visible(element);
      const observation = { width, height, visible: shown, passed: false };
      if (kind === 'document-horizontal-overflow') {
        observation.scrollWidth = element.scrollWidth; observation.clientWidth = element.clientWidth;
        observation.passed = element.scrollWidth <= element.clientWidth + 1;
      } else if (kind === 'hidden-navigation-track') {
        // The selector names the track container, not merely a hidden child.
        observation.hidden = !shown;
        observation.passed = shown || (width <= 1 && height <= 1);
      } else if (kind === 'primary-content-width') {
        observation.minimum = Math.max(Number(assertion.minWidth || 0), innerWidth * Number(assertion.minViewportRatio || 0));
        observation.passed = shown && observation.minimum > 0 && width >= observation.minimum;
      } else if (kind === 'initial-viewport-placement') {
        observation.top = rect.top; observation.bottom = rect.bottom;
        observation.passed = shown && rect.bottom > 0 && rect.top < innerHeight && rect.right > 0 && rect.left < innerWidth;
      } else {
        let clipped = element.scrollWidth > element.clientWidth + 1 && ['hidden', 'clip'].includes(style.overflowX);
        let ancestor = element.parentElement;
        while (ancestor) {
          const parentStyle = getComputedStyle(ancestor), parent = ancestor.getBoundingClientRect();
          if (['hidden', 'clip'].includes(parentStyle.overflowX) && (rect.left < parent.left - 1 || rect.right > parent.right + 1)) clipped = true;
          if (['hidden', 'clip'].includes(parentStyle.overflowY) && (rect.top < parent.top - 1 || rect.bottom > parent.bottom + 1)) clipped = true;
          ancestor = ancestor.parentElement;
        }
        const fontSize = parseFloat(style.fontSize);
        const lines = new Map();
        const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT);
        let node, characters = 0;
        while ((node = walker.nextNode()) && characters < 512) {
          for (let index = 0; index < node.length && characters < 512; index++) {
            if (!node.textContent[index].trim()) continue;
            const range = document.createRange(); range.setStart(node, index); range.setEnd(node, index + 1);
            const bounds = range.getBoundingClientRect(); if (!bounds.width || !bounds.height) continue;
            const line = Math.round(bounds.top / Math.max(1, fontSize * .3));
            lines.set(line, (lines.get(line) || 0) + 1); characters++;
          }
        }
        const characterWrapped = lines.size >= 3 && [...lines.values()].filter(count => count <= 2).length > lines.size / 2;
        Object.assign(observation, { clipped, fontSize, lines: lines.size, characters, characterWrapped });
        if (kind === 'clipping') observation.passed = shown && !clipped;
        else if (kind === 'no-character-wrapping') observation.passed = shown && characters > 0 && !characterWrapped;
        else if (['readable-heading', 'readable-canonical-identifier'].includes(kind)) observation.passed = shown && fontSize >= 12 && width >= fontSize * 3 && characters > 0 && !clipped && !characterWrapped;
        else return { ...observation, unknownKind: true };
      }
      return observation;
    });
    result.measurements = { elements: observations };
    result.result = observations.some(item => item.unknownKind) ? 'incomplete' : observations.every(item => item.passed) ? 'passed' : 'failed';
    return result;
  });
  const dataShapes = shapes.map(shape => {
    let present = false;
    try { present = Array.isArray(shape.conditionalDom) && shape.conditionalDom.length > 0 && shape.conditionalDom.every(selector => document.querySelector(selector)); } catch {}
    return { id: shape.id, revision: shape.revision, result: shape.id && shape.revision && shape.layoutEffect && present ? 'passed' : 'incomplete' };
  });
  return { assertions: measured, dataShapes };
}

export function formalReceipt(report, exitCode, blocking) {
  const pages = report.pages || [], reasons = [];
  const digest = file => createHash('sha256').update(fs.readFileSync(file)).digest('hex');
  const retained = item => {
    try { return item?.path && /^[a-f0-9]{64}$/.test(item.sha256) && digest(item.path) === item.sha256; } catch { return false; }
  };
  let blocked = false, failed = blocking.length > 0;
  if (!report.coverage?.readinessEligible || report.coverage?.failed || pages.length !== report.plan?.plannedPageCount) reasons.push('incomplete-required-coverage');
  if (!pages.length) reasons.push('no-rendered-cells');
  for (const page of pages) {
    if (page.cache?.hit || page.skipped) reasons.push('cached-or-skipped-cell');
    if (page.outcome !== 'checked' || !retained(page.screenshots?.viewport) || !retained(page.screenshots?.fullPage)) blocked = true;
    const layout = page.metrics?.declaredLayout;
    if (!layout?.dataShapes?.length || layout.dataShapes.some(item => item.result !== 'passed')) reasons.push('missing-data-shape-evidence');
    if (!layout?.assertions?.length || requiredGeometryKinds.some(kind => !layout.assertions.some(item => item.kind === kind)) || layout.assertions.some(item => item.result === 'incomplete')) reasons.push('missing-geometry-evidence');
    if (layout?.assertions?.some(item => item.result === 'failed')) failed = true;
  }
  if (!retained({path:report.review?.queuePath,sha256:report.review?.queueSha256}) || !retained(report.evidence?.journey)) blocked = true;
  const result = blocked ? 'blocked' : failed ? 'failed' : exitCode !== 0 || reasons.length ? 'incomplete' : 'passed';
  const manifest = {
    runId: report.runId, verifier: report.evidence?.verifier, config: report.evidence?.config,
    journey: report.evidence?.journey, reviewQueueSha256: report.review?.queueSha256,
    cells: pages.map(page => ({ cellId: page.cellId, source: page.review?.sourceFingerprint, intent: page.review?.intentFingerprint, screenshots: page.screenshots, layout: page.metrics?.declaredLayout })),
  };
  return { result, runId: report.runId, manifestSha256: createHash('sha256').update(JSON.stringify(manifest)).digest('hex'), reasons: [...new Set(reasons)], checkedCells: pages.filter(page => page.outcome === 'checked').length };
}
