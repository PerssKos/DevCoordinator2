import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { after, before, test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { pageVerifier } from "../../../skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs";

const require = createRequire(import.meta.url);
const moduleRoot = process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES;
const { chromium } = moduleRoot
  ? require(`${moduleRoot}/playwright`)
  : require("../../../ci/playwright/node_modules/playwright");
let browser;

before(async () => { browser = await chromium.launch({ headless: true }); });
after(async () => { await browser?.close(); });

function editorFixture(extraStyle = "") {
  return `<!doctype html><meta charset="utf-8"><style>
    body { margin: 0; color: #111; background: white; }
    main { padding: 12px; }
    .cm-scroller { display: flex; position: relative; width: 170px; height: 104px; overflow: auto; font: 14px/24px monospace; }
    .cm-gutters { position: sticky; left: 0; z-index: 2; flex: none; width: 42px; background: #eee; }
    .cm-gutterElement { height: 24px; }
    .cm-content { flex: none; }
    .cm-line { width: 820px; height: 24px; white-space: pre; }
    .long-line { box-sizing: border-box; padding-left: 560px; }
    ${extraStyle}
  </style><main><div class="cm-scroller">
    <div class="cm-gutters"><div class="cm-gutter">${Array.from({ length: 12 }, (_, index) => `<div class="cm-gutterElement">${index + 1}</div>`).join("")}</div></div>
    <div class="cm-content"><div class="cm-line long-line"><span id="probe-away">long_identifier</span></div>
    ${Array.from({ length: 11 }, (_, index) => `<div class="cm-line"><span id="source-${index}">logic</span> <span>signal_${index};</span></div>`).join("")}</div>
  </div></main>`;
}

async function scrollCoordinates(page) {
  return page.evaluate(() => {
    const entries = [{ selector: "window", left: window.scrollX, top: window.scrollY }];
    const roots = [document];
    while (roots.length) {
      for (const element of roots.shift().querySelectorAll("*")) {
        if (element.shadowRoot) roots.push(element.shadowRoot);
        if (element.scrollWidth > element.clientWidth || element.scrollHeight > element.clientHeight) {
          entries.push({ selector: element.id || element.className || element.tagName, left: element.scrollLeft, top: element.scrollTop });
        }
      }
    }
    return entries;
  });
}

async function measure(page) {
  await page.evaluate(() => {
    window.__FORMAL_WEB_UI_CONFIG__ = { rules: { strictTruncation: false }, inspectThemePalette: false };
  });
  return page.evaluate(pageVerifier);
}

function occlusions(report) {
  return report.findings.filter((finding) => ["occluded", "partially-occluded"].includes(finding.rule));
}

function contextualOverlayFixture({ declared = true, reason = "User-opened preview", innerCover = false, positioned = true } = {}) {
  return `<!doctype html><style>
    body{margin:0;background:white;color:#111}
    #background{position:absolute;left:30px;top:30px;width:150px;height:34px}
    #popup{position:${positioned ? "fixed" : "relative"};left:12px;top:12px;width:250px;height:180px;background:#eee;z-index:2}
    #popup h2{margin:0;padding:12px;font:16px sans-serif}
    #inside{position:absolute;left:18px;top:70px;width:150px;height:34px}
    #inner-cover{position:absolute;left:18px;top:70px;width:150px;height:34px;z-index:3;background:#333}
  </style><button id="background">Source action</button>
  <section id="popup" role="dialog" aria-label="Preview" ${declared ? `data-ui-contextual-overlay="${reason}"` : ""}>
    <h2>Preview</h2><button id="inside">Edit inputs</button>${innerCover ? '<div id="inner-cover"></div>' : ""}
  </section>`;
}

for (const width of [390, 1440]) {
  test(`contextual overlay only excuses declared background coverage: ${width}`, async () => {
    const page = await browser.newPage({ viewport: { width, height: 844 } });
    try {
      await page.setContent(contextualOverlayFixture());
      const report = await measure(page);
      assert.deepEqual(occlusions(report), []);
      assert(report.findings.some(finding => finding.rule === "allowed-contextual-overlay" && finding.selector.includes("background")));
    } finally { await page.close(); }
  });
  test(`contextual overlay still detects covered controls within it: ${width}`, async () => {
    const page = await browser.newPage({ viewport: { width, height: 844 } });
    try {
      await page.setContent(contextualOverlayFixture({ innerCover: true }));
      const report = await measure(page);
      assert(occlusions(report).some(finding => finding.selector.includes("inside") && finding.severity === "critical"));
    } finally { await page.close(); }
  });
}

for (const options of [{ declared: false }, { reason: "" }, { positioned: false }]) {
  test(`contextual overlay requires an explicit positioned declaration: ${JSON.stringify(options)}`, async () => {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    try {
      await page.setContent(contextualOverlayFixture(options));
      const report = await measure(page);
      assert(occlusions(report).some(finding => finding.selector.includes("background")));
    } finally { await page.close(); }
  });
}

test("ordinary positioned content cannot declare itself a contextual surface", async () => {
  const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
  try {
    await page.setContent(contextualOverlayFixture().replace('role="dialog"', 'role="region"'));
    const report = await measure(page);
    assert(occlusions(report).some(finding => finding.selector.includes("background")));
  } finally { await page.close(); }
});

for (const width of [390, 1440]) {
  for (const axis of ["x", "y", "both"]) {
    test(`scrolled non-fixed editor content remains reachable: ${width}, ${axis}`, async () => {
      const page = await browser.newPage({ viewport: { width, height: 844 } });
      try {
        await page.setContent(editorFixture());
        await page.locator(".cm-scroller").evaluate((element, axis) => {
          element.scrollLeft = axis !== "y" ? 64 : 0;
          element.scrollTop = axis !== "x" ? 80 : 0;
        }, axis);
        const before = await scrollCoordinates(page);
        const report = await measure(page);
        assert.deepEqual(report.findings.filter(finding => finding.rule === "offcanvas-cut"), []);
        assert.deepEqual(await scrollCoordinates(page), before);
      } finally { await page.close(); }
    });
  }
}

for (const axis of ["x", "y"]) {
  test(`genuinely unreachable negative content is not excused by a scroll container: ${axis}`, async () => {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    try {
      await page.setContent(`<!doctype html><style>body{margin:0;color:#111;background:white}.scroll{position:relative;margin:12px;width:200px;height:150px;overflow:auto}.content{width:600px;height:600px}.bad{position:absolute;left:${axis === "x" ? "-30px" : "10px"};top:${axis === "y" ? "-30px" : "10px"};width:90px;height:50px}</style><div class="scroll"><div class="content"><button class="bad">Unreachable action</button></div></div>`);
      const report = await measure(page);
      assert(report.findings.some(finding => finding.rule === "offcanvas-cut" && finding.severity === "critical" && finding.selector.includes("bad")), JSON.stringify(report.findings));
    } finally { await page.close(); }
  });
}

for (const width of [390, 1440]) {
  for (const left of [0, 64]) {
    test(`pinned gutter stays reachable and restores every scroll coordinate: width ${width}, left ${left}`, async () => {
      const page = await browser.newPage({ viewport: { width, height: 844 } });
      try {
        await page.setContent(editorFixture());
        await page.locator(".cm-scroller").evaluate((element, offset) => { element.scrollLeft = offset; }, left);
        const before = await scrollCoordinates(page);
        const report = await measure(page);
        const after = await scrollCoordinates(page);
        assert.deepEqual({ scroll: after, occlusions: occlusions(report) }, { scroll: before, occlusions: [] });
      } finally {
        await page.close();
      }
    });
  }
}

test("nested and shadow-root scroll positions survive probes and smooth CSS", async () => {
  const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
  try {
    await page.setContent(editorFixture("html { scroll-behavior: smooth; } body { width: 1100px; height: 1700px; } .cm-scroller { scroll-behavior: smooth; }"));
    await page.evaluate((fixture) => {
      const host = document.createElement("section");
      host.id = "shadow-editor";
      host.style.cssText = "display:block; width:220px; height:120px; overflow:auto; margin:24px 180px";
      document.body.append(host);
      host.attachShadow({ mode: "open" }).innerHTML = fixture;
      host.shadowRoot.querySelector(".cm-scroller").scrollTo({ left: 64, top: 48, behavior: "instant" });
      host.scrollTo({ left: 0, top: 10, behavior: "instant" });
      document.querySelector(".cm-scroller").scrollTo({ left: 64, top: 24, behavior: "instant" });
      window.scrollTo({ left: 110, top: 20, behavior: "instant" });
    }, editorFixture());
    const before = await scrollCoordinates(page);
    await measure(page);
    await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    assert.deepEqual(await scrollCoordinates(page), before);
  } finally {
    await page.close();
  }
});

for (const obstruction of ["oversized-gutter", "absolute-overlay", "fixed-overlay", "non-edge-sticky"]) {
  test(`unreachable source is still critical: ${obstruction}`, async () => {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    try {
      await page.setContent(editorFixture(obstruction === "oversized-gutter" ? ".cm-gutters { width: 190px; }" : ""));
      if (obstruction !== "oversized-gutter") {
        await page.evaluate((kind) => {
          const source = document.querySelector("#source-0");
          const rect = source.getBoundingClientRect();
          const cover = document.createElement("div");
          cover.id = "unrelated-cover";
          if (kind === "fixed-overlay") {
            cover.style.cssText = `position:fixed;left:${rect.left}px;top:${rect.top}px;width:85px;height:24px;background:#ddd;z-index:20`;
            document.body.append(cover);
          } else {
            cover.style.cssText = `position:${kind === "non-edge-sticky" ? "sticky" : "absolute"};left:42px;top:24px;width:85px;height:24px;background:#ddd;z-index:20;flex:none;margin-left:-820px`;
            if (kind === "absolute-overlay") cover.style.marginLeft = "0";
            document.querySelector(".cm-scroller").append(cover);
          }
        }, obstruction);
      }
      const report = await measure(page);
      assert(occlusions(report).some((finding) => finding.severity === "critical" && finding.selector.startsWith("#source-")), JSON.stringify(report.findings));
    } finally {
      await page.close();
    }
  });
}

test("non-scrollable clipped content stays critical", async () => {
  const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
  try {
    await page.setContent('<main style="width:70px;overflow:hidden"><span style="white-space:nowrap;color:#111;background:#fff">source_identifier_that_cannot_be_reached</span></main>');
    const report = await measure(page);
    assert(report.findings.some((finding) => finding.severity === "critical" && finding.rule === "clipped-by-ancestor"));
  } finally {
    await page.close();
  }
});

test("a failing probe still restores every scroll coordinate", async () => {
  const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
  try {
    await page.setContent(editorFixture());
    const before = await scrollCoordinates(page);
    await page.evaluate(() => {
      const original = Element.prototype.scrollIntoView;
      Element.prototype.scrollIntoView = function (...argumentsList) {
        original.apply(this, argumentsList);
        throw new Error("intentional-probe-failure");
      };
    });
    await assert.rejects(measure(page), /intentional-probe-failure/);
    assert.deepEqual(await scrollCoordinates(page), before);
  } finally {
    await page.close();
  }
});

test("an edge-pinned gutter cannot excuse source outside its scroll range", async () => {
  const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
  try {
    await page.setContent(editorFixture(".cm-content { margin-left: -42px; }"));
    const report = await measure(page);
    assert(occlusions(report).some((finding) => finding.severity === "critical" && finding.selector === "#source-0"));
  } finally {
    await page.close();
  }
});

test("right-pinned sibling reachability preserves a nonzero offset", async () => {
  const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
  try {
    await page.setContent(editorFixture(".cm-gutters { left: auto; right: 0; order: 2; }"));
    await page.locator(".cm-scroller").evaluate((element) => { element.scrollLeft = 495; });
    const before = await scrollCoordinates(page);
    const report = await measure(page);
    assert.deepEqual({ scroll: await scrollCoordinates(page), occlusions: occlusions(report) }, { scroll: before, occlusions: [] });
  } finally {
    await page.close();
  }
});

const fixturesRoot = new URL("../../../skills/formal-web-ui-verification/fixtures/self-test/", import.meta.url);
const pages = JSON.parse(fs.readFileSync(new URL("pages.json", fixturesRoot), "utf8"));
const matrix = JSON.parse(fs.readFileSync(new URL("matrix.json", fixturesRoot), "utf8"));
for (const covered of [false, true]) {
  test(`document popup probe preserves the original heading: covered ${covered}`, async () => {
    const page = await browser.newPage({ viewport: { width: 390, height: 921 } });
    try {
      await page.setContent(pages[`popup-scroll-probe-${covered ? "covered" : "clean"}.html`]);
      const before = await scrollCoordinates(page);
      const report = await measure(page);
      assert.deepEqual(await scrollCoordinates(page), before);
      if (covered) {
        assert(occlusions(report).some((finding) => finding.selector === "#page-heading" && finding.severity === "critical"));
      } else {
        assert.equal(occlusions(report).length, 0);
      }
    } finally {
      await page.close();
    }
  });
}

test("the gutter itself is not exempt from unrelated occlusion", async () => {
  const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
  try {
    await page.setContent(editorFixture());
    await page.evaluate(() => {
      const gutter = document.querySelector(".cm-gutterElement");
      gutter.id = "protected-gutter";
      const rect = gutter.getBoundingClientRect();
      const cover = document.createElement("div");
      cover.style.cssText = `position:fixed;left:${rect.left}px;top:${rect.top}px;width:${rect.width}px;height:${rect.height}px;background:#ddd;z-index:20`;
      document.body.append(cover);
    });
    const report = await measure(page);
    assert(occlusions(report).some((finding) => finding.selector === "#protected-gutter" && finding.severity === "critical"));
  } finally {
    await page.close();
  }
});

const selectedCases = new Set(["occluded", "partial-overlap", "overlay-in-scroll", "scroll-panel-neighbor", "details-closed-neighbor", "dev-overlay-badge", "modal-active-occlusion", "modal-shadow-occlusion", "modal-scroll-locked", "fixed-unscrollable-cut"]);
for (const fixture of matrix.cases.filter((entry) => selectedCases.has(entry.name) || entry.name.startsWith("custom-modal-"))) {
  test(`existing detector guard: ${fixture.name}`, async () => {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    try {
      await page.setContent(pages[fixture.path]);
      const report = await measure(page);
      for (const rule of fixture.critical_rules || []) {
        assert(report.findings.some((finding) => finding.rule === rule && finding.severity === "critical"), `${fixture.name} missed ${rule}`);
      }
      for (const rule of fixture.forbidden_rules || []) {
        assert(!report.findings.some((finding) => finding.rule === rule), `${fixture.name} falsely reported ${rule}`);
      }
      if (!fixture.critical) assert(!report.findings.some((finding) => finding.severity === "critical"), JSON.stringify(report.findings));
    } finally {
      await page.close();
    }
  });
}

test("retained candidate CLI verifies the rendered editor at both widths", () => {
  const evidence = fs.mkdtempSync(path.join(os.tmpdir(), "formal-occlusion-proof-"));
  const fixture = path.join(evidence, "editor.html");
  const config = path.join(evidence, "config.json");
  const reportPath = path.join(evidence, "report.json");
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  fs.writeFileSync(fixture, editorFixture());
  fs.writeFileSync(config, JSON.stringify({
    repoRoot: root,
    targetDefaults: {
      journeys: [{ id: "source", frequencyPercent: 100, risk: "normal" }],
      primaryJourney: "source",
      regions: [{ selector: "main", role: "primary-content", journey: "source" }],
      theme: "light",
      reviewInputs: [{ path: "rust/tooling/tests/formal_occlusion.mjs", kind: "ui-code" }],
    },
    targets: [{ name: "pinned-source", url: pathToFileURL(fixture).href }],
    viewports: [{ name: "compact", width: 390, height: 844 }, { name: "wide", width: 1440, height: 900 }],
    scroll: false,
  }));
  const environment = { ...process.env, TMPDIR: evidence };
  delete environment.DEVCOORDINATOR_EVIDENCE_DIR;
  delete environment.DEVCOORDINATOR_RUN_ID;
  delete environment.DEVCOORDINATOR_CHECK_NAME;
  const run = spawnSync(process.execPath, [
    path.join(root, "skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs"),
    "--config", config, "--json-out", reportPath, "--markdown-out", path.join(evidence, "report.md"),
    "--playwright-module-dir", moduleRoot || path.join(root, "ci/playwright/node_modules"),
  ], { encoding: "utf8", env: environment, timeout: 60000 });
  fs.writeFileSync(path.join(evidence, "stdout.json"), run.stdout || "");
  fs.writeFileSync(path.join(evidence, "stderr.log"), run.stderr || "");
  assert.equal(run.status, 0, `retained verifier failed; evidence: ${evidence}`);
  const report = JSON.parse(fs.readFileSync(reportPath, "utf8"));
  assert.equal(report.pages.length, 2);
  assert.equal(occlusions(report).length, 0);
  assert(!report.findings.some((finding) => finding.severity === "critical"));
  process.stdout.write(`Retained CLI proof: ${evidence}\n`);
});
