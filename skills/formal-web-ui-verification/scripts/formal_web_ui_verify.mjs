#!/usr/bin/env node
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createHash, randomBytes } from "node:crypto";
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const VERIFIER_PATH = fileURLToPath(import.meta.url);
const SEVERITY_ORDER = { info: 0, warning: 1, critical: 2 };
const DEFAULT_VIEWPORTS = [
  { name: "mobile", width: 390, height: 844 },
  { name: "desktop", width: 1440, height: 900 },
];
const DEFAULT_MAX_PAGE_COUNT = 60;
const DEFAULT_MAX_CONCURRENCY = 4;
const MAX_DELAY_INTERVAL_MS = 100;
const DEFAULT_RENDERED_PERFORMANCE = Object.freeze({
  ttfbMs: 10,
  lcpMs: 800,
  ttfbLocalOnly: true,
});
const RECEIPT_MAX_BYTES = 2048;
const DEFAULT_ARTIFACT_PREFIX = "formal-web-ui-verification-";
const REPORT_SCHEMA_VERSION = 2;
const REVIEW_QUEUE_SCHEMA_VERSION = 1;
const JOURNEY_EVIDENCE_SCHEMA_VERSION = 1;
const JOURNEY_EVIDENCE_KIND = "formal-web-ui-journey-evidence";
const GOVERNED_BUNDLE_DIRECTORY = "formal-runs";
const MANUAL_REVIEW_SCHEMA_VERSION = 1;
const MANUAL_REVIEW_KIND = "formal-web-ui-manual-review";
const REVIEW_QUEUE_KIND = "formal-web-ui-review-queue";
const SHA256_RE = /^[0-9a-f]{64}$/;
const SCREENSHOT_REDACTION_STYLE = `
input:not([type="checkbox"]):not([type="radio"]):not([type="range"]):not([type="color"]),
textarea,
select {
  color: transparent !important;
  -webkit-text-fill-color: transparent !important;
  text-shadow: none !important;
  caret-color: transparent !important;
}
input::placeholder,
textarea::placeholder {
  color: transparent !important;
  -webkit-text-fill-color: transparent !important;
  text-shadow: none !important;
}
`;

let fallbackArtifacts;
let activeArtifacts;
let activeConfigSha256 = null;
const runStartedAt = new Date().toISOString();
const verifierSha256 = createHash("sha256").update(fs.readFileSync(VERIFIER_PATH)).digest("hex");

function usage() {
  return `Usage:
  node scripts/formal_web_ui_verify.mjs --url <url> [--viewport name=390x844] [--json-out out.json] [--markdown-out out.md]
  node scripts/formal_web_ui_verify.mjs --config formal-web-ui.json
  node scripts/formal_web_ui_verify.mjs --from-coordinator --coordinator-command devcoordinator2 --only-current

Options:
  --url <url>                       Add a URL target. Requires complete targetDefaults from --config.
  --config <path>                   Load JSON config.
  --viewport <name=WIDTHxHEIGHT>    Add viewport. Can be repeated.
  --max-page-count <count>          Hard cap on route/state/viewport cells. Default: ${DEFAULT_MAX_PAGE_COUNT}.
  --concurrency <count>             Maximum concurrently running safe cells. Default: ${DEFAULT_MAX_CONCURRENCY}.
  --changed-path <path>             Select affected cells for a development-only run. Repeatable.
  --cache-dir <path>                Explicit external development cache directory.
  --data-revision <value>           Caller-owned fixture/data identity required with --cache-dir.
  --repo-root <path>                Repository root for declared per-target UI review inputs.
  --review-against <path>           Explicit prior reviewed manifest for changed-input review selection.
  --json-out <path>                 Override the auto-created JSON artifact path.
  --markdown-out <path>             Override the auto-created Markdown artifact path.
  --review-queue-out <path>         Override the generated changed-visual-review queue path.
  --journey-evidence-out <path>     Override the retained Console journey-evidence manifest path.
  --progress-out <path>             Override the bounded JSON-lines progress artifact path.
  --receipt-only                    Deprecated no-op; bounded receipt output is already the default.
  --human-readable-stdout           Human-only compatibility mode: print the full Markdown report instead of the bounded JSON receipt.
  --fail-on <critical|warning|info> Exit 1 when this severity or higher is found. Default: critical.
  --browser-executable <path>       Use a specific Chrome/Chromium executable.
  --playwright-module-dir <path>    Resolve Playwright from this explicit node_modules directory.
  --from-coordinator                Read current loopback routes from DevCoordinator2.
  --coordinator-command <path>      Coordinator executable for --from-coordinator. Default: devcoordinator2.
  --coordinator-project <path>      Optional repository path filter for --from-coordinator.
  --only-current                    With --from-coordinator, skip stale/stopped/reused URLs.
  --allow-discovered-target-failures
                                      Tolerate failed coordinator-discovered URLs while reporting them.
  --area <name=selector>            Add an area of interest.
  --ignore <selector=reason>        Ignore selector with reason.
  --allow-truncation <selector=reason>
  --allow-overlap <selector=reason>
  --screenshot-dir <path>           Override the automatic initial/full-page screenshot directory.
  --no-scroll                       Skip the full-page scroll pass (default: scroll on).
  --cookie <name=value>             Send a cookie with every target (repeatable). Use for
                                    auth-gated pages; scoped to the target URL by default.
  --ignore-https-errors             Accept invalid/self-signed TLS certificates.
`;
}

function pathIsWithin(candidate, root) {
  const relative = path.relative(root, candidate);
  return relative === "" || (!relative.startsWith(`..${path.sep}`) && relative !== ".." && !path.isAbsolute(relative));
}

function createDefaultArtifacts() {
  const cwd = fs.realpathSync.native(process.cwd());
  const governedEvidence = process.env.DEVCOORDINATOR_EVIDENCE_DIR;
  if (governedEvidence) {
    if (!path.isAbsolute(governedEvidence)) {
      throw new Error("DEVCOORDINATOR_EVIDENCE_DIR must be absolute");
    }
    fs.mkdirSync(governedEvidence, { recursive: true, mode: 0o700 });
    const directory = fs.realpathSync.native(governedEvidence);
    fs.chmodSync(directory, 0o700);
    return {
      directory,
      jsonOut: path.join(directory, "report.json"),
      markdownOut: path.join(directory, "report.md"),
      reviewQueueOut: path.join(directory, "review-queue.json"),
      journeyEvidenceOut: path.join(directory, "journey-evidence.json"),
      progressOut: path.join(directory, "progress.jsonl"),
      screenshotDir: path.join(directory, "screenshots"),
      automatic: true,
      governed: true,
    };
  }
  const roots = [os.tmpdir(), path.join(os.homedir(), ".cache")];
  let lastError;
  for (const candidate of [...new Set(roots.map((item) => path.resolve(item)))]) {
    try {
      fs.mkdirSync(candidate, { recursive: true, mode: 0o700 });
      const realRoot = fs.realpathSync.native(candidate);
      if (pathIsWithin(realRoot, cwd)) continue;
      const directory = fs.mkdtempSync(path.join(realRoot, DEFAULT_ARTIFACT_PREFIX));
      fs.chmodSync(directory, 0o700);
      return {
        directory,
        jsonOut: path.join(directory, "report.json"),
        markdownOut: path.join(directory, "report.md"),
        reviewQueueOut: path.join(directory, "review-queue.json"),
        journeyEvidenceOut: path.join(directory, "journey-evidence.json"),
        progressOut: path.join(directory, "progress.jsonl"),
        screenshotDir: path.join(directory, "screenshots"),
        automatic: true,
      };
    } catch (error) {
      lastError = error;
    }
  }
  throw new Error(`Unable to create an external artifact directory: ${lastError?.message || "no safe writable root"}`);
}

function normalizeOutputPath(value, optionName) {
  if (value === undefined) return undefined;
  if (typeof value !== "string" || !value.trim()) {
    throw new Error(`${optionName} must be a non-empty path string`);
  }
  return path.resolve(value);
}

function deriveCompanionPath(source, extension) {
  const parsed = path.parse(source);
  let candidate = path.join(parsed.dir, `${parsed.name}${extension}`);
  if (candidate === source) candidate = `${source}${extension}`;
  return candidate;
}

function resolveArtifactPaths(config, cli, defaults) {
  let jsonOut = normalizeOutputPath(cli.jsonOut ?? config.jsonOut, "jsonOut");
  let markdownOut = normalizeOutputPath(cli.markdownOut ?? config.markdownOut, "markdownOut");
  if (!jsonOut && !markdownOut) {
    return {
      ...defaults,
      reviewQueueOut: normalizeOutputPath(
        cli.reviewQueueOut ?? config.reviewQueueOut ?? defaults.reviewQueueOut,
        "reviewQueueOut",
      ),
      journeyEvidenceOut: normalizeOutputPath(
        cli.journeyEvidenceOut ?? config.journeyEvidenceOut ?? defaults.journeyEvidenceOut,
        "journeyEvidenceOut",
      ),
      progressOut: normalizeOutputPath(
        cli.progressOut ?? config.progressOut ?? defaults.progressOut,
        "progressOut",
      ),
      screenshotDir: normalizeOutputPath(
        cli.screenshotDir ?? config.screenshotDir ?? defaults.screenshotDir,
        "screenshotDir",
      ),
    };
  }
  if (!jsonOut) jsonOut = deriveCompanionPath(markdownOut, ".json");
  if (!markdownOut) markdownOut = deriveCompanionPath(jsonOut, ".md");
  if (jsonOut === markdownOut) {
    throw new Error("JSON and Markdown artifact paths must be distinct");
  }
  return {
    directory: path.dirname(jsonOut) === path.dirname(markdownOut) ? path.dirname(jsonOut) : undefined,
    jsonOut,
    markdownOut,
    reviewQueueOut: normalizeOutputPath(
      cli.reviewQueueOut ?? config.reviewQueueOut ?? path.join(path.dirname(jsonOut), "review-queue.json"),
      "reviewQueueOut",
    ),
    journeyEvidenceOut: normalizeOutputPath(
      cli.journeyEvidenceOut ?? config.journeyEvidenceOut ?? path.join(path.dirname(jsonOut), "journey-evidence.json"),
      "journeyEvidenceOut",
    ),
    progressOut: normalizeOutputPath(
      cli.progressOut ?? config.progressOut ?? path.join(path.dirname(jsonOut), "progress.jsonl"),
      "progressOut",
    ),
    screenshotDir: normalizeOutputPath(
      cli.screenshotDir ?? config.screenshotDir ?? path.join(path.dirname(jsonOut), "screenshots"),
      "screenshotDir",
    ),
    automatic: false,
  };
}

function removeUnusedDefaultArtifacts(defaults, selected) {
  if (!defaults || selected === defaults || !defaults.directory) return;
  try {
    fs.rmdirSync(defaults.directory);
  } catch {
    // A non-empty or concurrently replaced fallback is evidence worth preserving.
  }
}

function parseKeyValue(value, optionName) {
  const index = value.indexOf("=");
  if (index <= 0) {
    throw new Error(`${optionName} expects key=value, got ${value}`);
  }
  return [value.slice(0, index), value.slice(index + 1)];
}

function parseViewport(value) {
  const [name, dims] = parseKeyValue(value, "--viewport");
  const match = /^(\d+)x(\d+)$/i.exec(dims.trim());
  if (!match) {
    throw new Error(`--viewport expects name=WIDTHxHEIGHT, got ${value}`);
  }
  return { name, width: Number(match[1]), height: Number(match[2]) };
}

function parseSelectorReason(value, optionName) {
  const [selector, reason] = parseKeyValue(value, optionName);
  if (!selector.trim() || !reason.trim()) {
    throw new Error(`${optionName} requires both selector and reason`);
  }
  return { selector: selector.trim(), reason: reason.trim() };
}

function parseArgs(argv) {
  const cli = {
    urls: [],
    viewports: [],
    areas: [],
    ignore: [],
    allowTruncation: [],
    allowOverlap: [],
    failOn: undefined,
    configPath: undefined,
    jsonOut: undefined,
    markdownOut: undefined,
    humanReadableStdout: false,
    browserExecutable: process.env.FORMAL_WEB_UI_BROWSER || process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH,
    fromCoordinator: false,
    coordinatorCommand: undefined,
    coordinatorProject: undefined,
    onlyCurrent: false,
    allowDiscoveredTargetFailures: false,
    screenshotDir: undefined,
    noScroll: false,
    cookies: [],
    ignoreHttpsErrors: false,
    maxPageCount: undefined,
    playwrightModuleDir: undefined,
    repoRoot: undefined,
    reviewAgainst: undefined,
    reviewQueueOut: undefined,
    journeyEvidenceOut: undefined,
    progressOut: undefined,
    concurrency: undefined,
    changedPaths: [],
    cacheDir: undefined,
    dataRevision: undefined,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      if (i + 1 >= argv.length) throw new Error(`${arg} requires a value`);
      i += 1;
      return argv[i];
    };
    if (arg === "--help" || arg === "-h") {
      console.log(usage());
      process.exit(0);
    } else if (arg === "--url") {
      cli.urls.push(next());
    } else if (arg === "--config") {
      cli.configPath = next();
    } else if (arg === "--viewport") {
      cli.viewports.push(parseViewport(next()));
    } else if (arg === "--max-page-count") {
      cli.maxPageCount = next();
    } else if (arg === "--repo-root") {
      cli.repoRoot = next();
    } else if (arg === "--review-against") {
      cli.reviewAgainst = next();
    } else if (arg === "--json-out") {
      cli.jsonOut = next();
    } else if (arg === "--markdown-out") {
      cli.markdownOut = next();
    } else if (arg === "--review-queue-out") {
      cli.reviewQueueOut = next();
    } else if (arg === "--journey-evidence-out") {
      cli.journeyEvidenceOut = next();
    } else if (arg === "--progress-out") {
      cli.progressOut = next();
    } else if (arg === "--concurrency") {
      cli.concurrency = next();
    } else if (arg === "--changed-path") {
      cli.changedPaths.push(next());
    } else if (arg === "--cache-dir") {
      cli.cacheDir = next();
    } else if (arg === "--data-revision") {
      cli.dataRevision = next();
    } else if (arg === "--receipt-only") {
      // Deprecated compatibility alias. Receipt output is always the safe
      // default, and this flag can never disable artifact creation.
    } else if (arg === "--human-readable-stdout") {
      cli.humanReadableStdout = true;
    } else if (arg === "--fail-on") {
      cli.failOn = next();
    } else if (arg === "--browser-executable") {
      cli.browserExecutable = next();
    } else if (arg === "--playwright-module-dir") {
      cli.playwrightModuleDir = next();
    } else if (arg === "--from-coordinator") {
      cli.fromCoordinator = true;
    } else if (arg === "--coordinator-command") {
      cli.coordinatorCommand = next();
    } else if (arg === "--coordinator-project") {
      cli.coordinatorProject = next();
    } else if (arg === "--only-current") {
      cli.onlyCurrent = true;
    } else if (arg === "--allow-discovered-target-failures") {
      cli.allowDiscoveredTargetFailures = true;
    } else if (arg === "--area") {
      const [name, selector] = parseKeyValue(next(), "--area");
      cli.areas.push({ name: name.trim(), selector: selector.trim() });
    } else if (arg === "--ignore") {
      cli.ignore.push(parseSelectorReason(next(), "--ignore"));
    } else if (arg === "--allow-truncation") {
      cli.allowTruncation.push(parseSelectorReason(next(), "--allow-truncation"));
    } else if (arg === "--allow-overlap") {
      cli.allowOverlap.push(parseSelectorReason(next(), "--allow-overlap"));
    } else if (arg === "--screenshot-dir") {
      cli.screenshotDir = next();
    } else if (arg === "--no-scroll") {
      cli.noScroll = true;
    } else if (arg === "--cookie") {
      const [name, value] = parseKeyValue(next(), "--cookie");
      cli.cookies.push({ name: name.trim(), value });
    } else if (arg === "--ignore-https-errors") {
      cli.ignoreHttpsErrors = true;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  return cli;
}

function loadConfig(configPath) {
  if (!configPath) return {};
  const parsed = JSON.parse(fs.readFileSync(configPath, "utf8"));
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("Config must be a JSON object");
  }
  return parsed;
}

function normalizeCookieList(value) {
  if (!value) return [];
  if (!Array.isArray(value)) throw new Error("cookies must be an array");
  return value.map((item) => {
    if (typeof item === "string") {
      const [name, val] = parseKeyValue(item, "cookies");
      return { name: name.trim(), value: val };
    }
    if (item && typeof item === "object" && typeof item.name === "string" && typeof item.value === "string") {
      const normalized = { name: item.name.trim(), value: item.value };
      if (!normalized.name) throw new Error("cookies entries must have a non-empty name");
      // Validate optional scoping fields here so a malformed config fails
      // with a clear message instead of a confusing Playwright error later.
      for (const field of ["url", "domain", "path"]) {
        if (item[field] === undefined || item[field] === null) continue;
        if (typeof item[field] !== "string" || !item[field].trim()) {
          throw new Error(`cookies entry '${normalized.name}': ${field} must be a non-empty string`);
        }
        normalized[field] = item[field].trim();
      }
      return normalized;
    }
    throw new Error("cookies entries must be 'name=value' strings or {name, value, url?, domain?, path?}");
  });
}

function normalizeSelectorReasonList(value, name) {
  if (!value) return [];
  if (!Array.isArray(value)) throw new Error(`${name} must be an array`);
  return value.map((item) => {
    if (typeof item === "string") return { selector: item, reason: "configured selector" };
    if (item && typeof item === "object" && typeof item.selector === "string" && typeof item.reason === "string") {
      return { selector: item.selector, reason: item.reason };
    }
    throw new Error(`${name} entries must be strings or {selector, reason}`);
  });
}

function normalizeJourneyDefinitions(value, name) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new Error(`${name} must be an array`);
  const seen = new Set();
  return value.map((item, index) => {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new Error(`${name}[${index}] must be an object`);
    }
    if (typeof item.id !== "string" || !item.id.trim()) {
      throw new Error(`${name}[${index}].id must be a non-empty stable string`);
    }
    const id = item.id.trim();
    if (seen.has(id)) throw new Error(`${name} contains duplicate journey id ${id}`);
    seen.add(id);
    const frequencyPercent = Number(item.frequencyPercent);
    if (!Number.isFinite(frequencyPercent) || frequencyPercent < 0 || frequencyPercent > 100) {
      throw new Error(`${name}[${index}].frequencyPercent must be between 0 and 100`);
    }
    const risk = item.risk === undefined ? "normal" : item.risk;
    if (!["critical", "high", "normal", "low"].includes(risk)) {
      throw new Error(`${name}[${index}].risk must be critical, high, normal, or low`);
    }
    if (item.name !== undefined && (typeof item.name !== "string" || !item.name.trim())) {
      throw new Error(`${name}[${index}].name must be a non-empty string when present`);
    }
    if (item.rationale !== undefined && (typeof item.rationale !== "string" || !item.rationale.trim())) {
      throw new Error(`${name}[${index}].rationale must be a non-empty string when present`);
    }
    return {
      id,
      name: item.name?.trim() || id,
      frequencyPercent,
      risk,
      rationale: item.rationale?.trim() || "",
    };
  });
}

function normalizeJourneyRegions(value, name) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new Error(`${name} must be an array`);
  return value.map((item, index) => {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new Error(`${name}[${index}] must be an object`);
    }
    if (typeof item.selector !== "string" || !item.selector.trim()) {
      throw new Error(`${name}[${index}].selector must be a non-empty string`);
    }
    const role = item.role;
    if (!["primary-content", "workflow-surface", "supporting", "blocking-alert"].includes(role)) {
      throw new Error(`${name}[${index}].role must be primary-content, workflow-surface, supporting, or blocking-alert`);
    }
    if (item.journey !== undefined && (typeof item.journey !== "string" || !item.journey.trim())) {
      throw new Error(`${name}[${index}].journey must be a non-empty journey id when present`);
    }
    if (item.name !== undefined && (typeof item.name !== "string" || !item.name.trim())) {
      throw new Error(`${name}[${index}].name must be a non-empty string when present`);
    }
    if (item.reason !== undefined && (typeof item.reason !== "string" || !item.reason.trim())) {
      throw new Error(`${name}[${index}].reason must be a non-empty string when present`);
    }
    return {
      selector: item.selector.trim(),
      role,
      journey: item.journey?.trim() || null,
      name: item.name?.trim() || item.selector.trim(),
      reason: item.reason?.trim() || "",
    };
  });
}

function normalizeReviewInputs(value, name) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new Error(`${name} must be an array`);
  return value.map((item, index) => {
    const input = typeof item === "string" ? { path: item, kind: "shared" } : item;
    if (!input || typeof input !== "object" || Array.isArray(input)) {
      throw new Error(`${name}[${index}] must be a path string or {path, kind?}`);
    }
    if (typeof input.path !== "string" || !input.path.trim()) {
      throw new Error(`${name}[${index}].path must be a non-empty repository-relative path`);
    }
    if (input.kind !== undefined && (typeof input.kind !== "string" || !input.kind.trim())) {
      throw new Error(`${name}[${index}].kind must be a non-empty string when present`);
    }
    return { path: input.path.trim(), kind: input.kind?.trim() || "shared" };
  });
}

function normalizeTheme(value, name) {
  if (value === undefined || value === null) return null;
  if (!["light", "dark", "mixed"].includes(value)) {
    throw new Error(`${name} must be light, dark, or mixed`);
  }
  return value;
}

function normalizeContinuation(value, name) {
  if (value === undefined || value === null) return null;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${name} must be an object`);
  }
  const kind = value.kind === undefined ? "in-page" : value.kind;
  if (!["in-page", "navigation"].includes(kind)) {
    throw new Error(`${name}.kind must be in-page or navigation`);
  }
  if (typeof value.anchor !== "string" || !value.anchor.trim()) {
    throw new Error(`${name}.anchor must be a non-empty selector`);
  }
  if (value.focusWithin !== undefined && (typeof value.focusWithin !== "string" || !value.focusWithin.trim())) {
    throw new Error(`${name}.focusWithin must be a non-empty selector when present`);
  }
  if (kind === "navigation" && (typeof value.expectedPath !== "string" || !value.expectedPath.startsWith("/"))) {
    throw new Error(`${name}.expectedPath must be an absolute route path for navigation`);
  }
  if (kind === "in-page" && value.expectedPath !== undefined) {
    throw new Error(`${name}.expectedPath is valid only for navigation`);
  }
  const maxScrollDelta = value.maxScrollDelta === undefined ? 8 : Number(value.maxScrollDelta);
  if (!Number.isFinite(maxScrollDelta) || maxScrollDelta < 0) {
    throw new Error(`${name}.maxScrollDelta must be a non-negative number`);
  }
  const triggerActionIndex = value.triggerActionIndex === undefined ? null : Number(value.triggerActionIndex);
  if (triggerActionIndex !== null && (!Number.isInteger(triggerActionIndex) || triggerActionIndex < 0)) {
    throw new Error(`${name}.triggerActionIndex must be a non-negative integer when present`);
  }
  return {
    kind,
    anchor: value.anchor.trim(),
    focusWithin: value.focusWithin?.trim() || value.anchor.trim(),
    expectedPath: kind === "navigation" ? value.expectedPath : null,
    maxScrollDelta,
    triggerActionIndex,
  };
}

function normalizeRemovedReviewCells(value) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new Error("reviewRemovedCells must be an array");
  const seen = new Set();
  return value.map((item, index) => {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new Error(`reviewRemovedCells[${index}] must be {reviewCellKey, reason}`);
    }
    if (typeof item.reviewCellKey !== "string" || !item.reviewCellKey.trim()) {
      throw new Error(`reviewRemovedCells[${index}].reviewCellKey must be non-empty`);
    }
    if (typeof item.reason !== "string" || !item.reason.trim()) {
      throw new Error(`reviewRemovedCells[${index}].reason must be non-empty`);
    }
    const reviewCellKey = item.reviewCellKey.trim();
    if (seen.has(reviewCellKey)) throw new Error(`duplicate reviewRemovedCells key ${reviewCellKey}`);
    seen.add(reviewCellKey);
    return { reviewCellKey, reason: item.reason.trim() };
  });
}

function loadPriorManualReview(value) {
  if (value === undefined || value === null) return null;
  if (typeof value !== "string" || !value.trim()) {
    throw new Error("reviewAgainst must be a non-empty path string");
  }
  const reviewPath = path.resolve(value);
  const stat = fs.lstatSync(reviewPath);
  if (!stat.isFile() || stat.isSymbolicLink()) {
    throw new Error("reviewAgainst must name a regular non-symlink file");
  }
  const bytes = fs.readFileSync(reviewPath);
  const payload = JSON.parse(bytes.toString("utf8"));
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
    throw new Error("reviewAgainst must contain a manual-review JSON object");
  }
  if (payload.schemaVersion !== MANUAL_REVIEW_SCHEMA_VERSION || payload.kind !== MANUAL_REVIEW_KIND) {
    throw new Error("reviewAgainst is not a supported reviewed manifest");
  }
  for (const field of ["reviewedRunId", "reportSha256", "reviewQueueSha256"]) {
    if (typeof payload[field] !== "string" || !payload[field].trim()) {
      throw new Error(`reviewAgainst.${field} must be non-empty`);
    }
  }
  if (!SHA256_RE.test(payload.reportSha256) || !SHA256_RE.test(payload.reviewQueueSha256)) {
    throw new Error("reviewAgainst report and queue bindings must be SHA-256 values");
  }
  if (!Array.isArray(payload.decisions) || !payload.decisions.length) {
    throw new Error("reviewAgainst.decisions must be a non-empty array");
  }
  const seen = new Set();
  const decisions = payload.decisions.map((item, index) => {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new Error(`reviewAgainst.decisions[${index}] must be an object`);
    }
    if (typeof item.reviewCellKey !== "string" || !item.reviewCellKey.trim()) {
      throw new Error(`reviewAgainst.decisions[${index}].reviewCellKey must be non-empty`);
    }
    const reviewCellKey = item.reviewCellKey.trim();
    if (seen.has(reviewCellKey)) throw new Error(`reviewAgainst contains duplicate cell ${reviewCellKey}`);
    seen.add(reviewCellKey);
    if (!["pass", "gap", "blocked"].includes(item.decision)) {
      throw new Error(`reviewAgainst.decisions[${index}].decision must be pass, gap, or blocked`);
    }
    if (!SHA256_RE.test(item.sourceFingerprint || "") || !SHA256_RE.test(item.intentFingerprint || "")) {
      throw new Error(`reviewAgainst.decisions[${index}] requires source and intent SHA-256 fingerprints`);
    }
    const screenshots = item.screenshots;
    if (!screenshots || !SHA256_RE.test(screenshots.viewportSha256 || "") || !SHA256_RE.test(screenshots.fullPageSha256 || "")) {
      throw new Error(`reviewAgainst.decisions[${index}] requires both screenshot SHA-256 values`);
    }
    if (item.decision !== "pass" && (typeof item.note !== "string" || !item.note.trim())) {
      throw new Error(`reviewAgainst.decisions[${index}] requires a note for ${item.decision}`);
    }
    return { ...item, reviewCellKey };
  });
  return {
    path: reviewPath,
    sha256: sha256(bytes),
    reviewedRunId: payload.reviewedRunId,
    reportSha256: payload.reportSha256,
    reviewQueueSha256: payload.reviewQueueSha256,
    decisions,
  };
}

function normalizeContentInsetList(value, name) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new Error(`${name} must be an array`);
  return value.map((item, index) => {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new Error(`${name}[${index}] must be {selector, min, name?}`);
    }
    if (typeof item.selector !== "string" || !item.selector.trim()) {
      throw new Error(`${name}[${index}].selector must be a non-empty string`);
    }
    const min = Number(item.min);
    if (!Number.isFinite(min) || min <= 0) {
      throw new Error(`${name}[${index}].min must be a positive number of CSS pixels`);
    }
    if (item.name !== undefined && (typeof item.name !== "string" || !item.name.trim())) {
      throw new Error(`${name}[${index}].name must be a non-empty string when present`);
    }
    return {
      selector: item.selector.trim(),
      min,
      name: item.name?.trim() || item.selector.trim(),
    };
  });
}

function normalizeSourceBinding(value, name) {
  if (value === undefined || value === null) return null;
  const input = typeof value === "string" ? { expected: value } : value;
  if (!input || typeof input !== "object" || Array.isArray(input)) {
    throw new Error(`${name} must be an expected binding string or object`);
  }
  if (typeof input.expected !== "string" || !input.expected.trim()) {
    throw new Error(`${name}.expected must be a non-empty string`);
  }
  if (input.expected.trim().length > 512) {
    throw new Error(`${name}.expected must be 512 characters or fewer`);
  }
  const responseHeader = input.responseHeader === undefined
    ? "x-ui-source-revision"
    : input.responseHeader;
  const metaName = input.metaName === undefined
    ? "ui-source-revision"
    : input.metaName;
  if (responseHeader !== null && (
    typeof responseHeader !== "string" ||
    !/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(responseHeader)
  )) {
    throw new Error(`${name}.responseHeader must be null or a valid non-empty HTTP header name`);
  }
  if (metaName !== null && (typeof metaName !== "string" || !metaName.trim())) {
    throw new Error(`${name}.metaName must be null or a non-empty string`);
  }
  if (responseHeader === null && metaName === null) {
    throw new Error(`${name} must declare responseHeader, metaName, or both`);
  }
  return {
    expected: input.expected.trim(),
    responseHeader: responseHeader === null ? null : responseHeader.toLowerCase(),
    metaName: metaName === null ? null : metaName.trim(),
  };
}

function normalizeBreakpointProfile(value, name) {
  if (value === undefined || value === null) return null;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${name} must be {breakpoints, height, name?, baseViewport?}`);
  }
  if (!Array.isArray(value.breakpoints) || !value.breakpoints.length) {
    throw new Error(`${name}.breakpoints must be a non-empty array`);
  }
  const breakpoints = [...new Set(value.breakpoints.map((entry, index) => {
    const width = Number(entry);
    if (!Number.isInteger(width) || width < 2) {
      throw new Error(`${name}.breakpoints[${index}] must be an integer of at least 2 CSS pixels`);
    }
    return width;
  }))].sort((left, right) => left - right);
  const height = Number(value.height);
  if (!Number.isInteger(height) || height <= 0) {
    throw new Error(`${name}.height must be a positive integer`);
  }
  const profileName = value.name === undefined ? "responsive" : value.name;
  if (typeof profileName !== "string" || !profileName.trim()) {
    throw new Error(`${name}.name must be a non-empty string when present`);
  }
  if (value.baseViewport !== undefined && (
    typeof value.baseViewport !== "string" || !value.baseViewport.trim()
  )) {
    throw new Error(`${name}.baseViewport must be a non-empty viewport name when present`);
  }
  return {
    name: profileName.trim(),
    breakpoints,
    height,
    baseViewport: value.baseViewport?.trim(),
  };
}

function normalizeWaitFor(value, name) {
  if (!value) return {};
  if (typeof value === "string") {
    if (!value.trim()) throw new Error(`${name} selector must not be empty`);
    return { selector: value.trim() };
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${name} must be a selector string or object`);
  }
  const normalized = {};
  for (const key of ["selector", "errorSelector", "responseUrl", "url"]) {
    if (value[key] !== undefined) {
      if (typeof value[key] !== "string" || !value[key].trim()) {
        throw new Error(`${name}.${key} must be a non-empty string`);
      }
      normalized[key] = value[key].trim();
    }
  }
  if (value.loadState !== undefined) {
    if (!["load", "domcontentloaded", "networkidle"].includes(value.loadState)) {
      throw new Error(`${name}.loadState must be one of load, domcontentloaded, networkidle`);
    }
    normalized.loadState = value.loadState;
  }
  for (const key of ["timeoutMs", "loadStateTimeoutMs", "networkIdleMs", "settleMs", "pollIntervalMs"]) {
    if (value[key] !== undefined) {
      const numberValue = Number(value[key]);
      if (!Number.isFinite(numberValue) || numberValue < 0) {
        throw new Error(`${name}.${key} must be a non-negative number`);
      }
      normalized[key] = numberValue;
    }
  }
  for (const key of ["settleMs", "pollIntervalMs"]) {
    if ((normalized[key] || 0) > MAX_DELAY_INTERVAL_MS) {
      throw new Error(`${name}.${key} must not exceed ${MAX_DELAY_INTERVAL_MS} ms`);
    }
  }
  if (value.renderFrames !== undefined) {
    const renderFrames = Number(value.renderFrames);
    if (!Number.isInteger(renderFrames) || renderFrames < 1 || renderFrames > 2) {
      throw new Error(`${name}.renderFrames must be 1 or 2`);
    }
    normalized.renderFrames = renderFrames;
  }
  if (value.readback !== undefined) {
    const readback = value.readback;
    if (!readback || typeof readback !== "object" || Array.isArray(readback)) {
      throw new Error(`${name}.readback must be an object`);
    }
    if (typeof readback.url !== "string" || !readback.url.trim()) {
      throw new Error(`${name}.readback.url must be a non-empty string`);
    }
    const status = readback.status === undefined ? 200 : Number(readback.status);
    if (!Number.isInteger(status) || status < 100 || status > 599) {
      throw new Error(`${name}.readback.status must be an HTTP status integer`);
    }
    const intervalMs = readback.intervalMs === undefined ? 50 : Number(readback.intervalMs);
    if (!Number.isFinite(intervalMs) || intervalMs < 0 || intervalMs > MAX_DELAY_INTERVAL_MS) {
      throw new Error(`${name}.readback.intervalMs must be between 0 and ${MAX_DELAY_INTERVAL_MS}`);
    }
    if (readback.jsonPath !== undefined && (typeof readback.jsonPath !== "string" || !readback.jsonPath.trim())) {
      throw new Error(`${name}.readback.jsonPath must be a non-empty string when present`);
    }
    if ((readback.jsonPath === undefined) !== (readback.equals === undefined)) {
      throw new Error(`${name}.readback.jsonPath and equals must be supplied together`);
    }
    normalized.readback = {
      url: readback.url.trim(),
      status,
      intervalMs,
      ...(readback.jsonPath === undefined
        ? {}
        : { jsonPath: readback.jsonPath.trim(), equals: readback.equals }),
    };
  }
  return normalized;
}

function mergeWaitFor(configWaitFor, targetWaitFor) {
  return {
    ...normalizeWaitFor(configWaitFor, "waitFor"),
    ...normalizeWaitFor(targetWaitFor, "target.waitFor"),
  };
}

function timedWaitPromise(promise) {
  return promise.then((value) => ({ value, error: null }), (error) => ({ value: null, error }));
}

function armWaitFor(page, waitFor) {
  const timeout = waitFor.timeoutMs ?? 10000;
  return {
    response: waitFor.responseUrl
      ? timedWaitPromise(page.waitForResponse(waitFor.responseUrl, { timeout }))
      : null,
    url: waitFor.url
      ? timedWaitPromise(page.waitForURL(waitFor.url, { timeout }))
      : null,
  };
}

function jsonPathValue(payload, jsonPath) {
  let current = payload;
  for (const part of jsonPath.split(".")) {
    if (!part || current === null || typeof current !== "object" || !Object.hasOwn(current, part)) {
      return { found: false, value: undefined };
    }
    current = current[part];
  }
  return { found: true, value: current };
}

async function waitForReadback(page, readback, timeoutMs) {
  const started = Date.now();
  let attempts = 0;
  let lastStatus = null;
  while (Date.now() - started <= timeoutMs) {
    attempts += 1;
    try {
      const requestRemaining = Math.max(1, timeoutMs - (Date.now() - started));
      const response = await page.context().request.get(readback.url, { timeout: requestRemaining });
      lastStatus = response.status();
      if (lastStatus === readback.status) {
        if (!readback.jsonPath) {
          return { attempts, status: lastStatus };
        }
        const payload = await response.json();
        const observed = jsonPathValue(payload, readback.jsonPath);
        if (observed.found && stableJson(observed.value) === stableJson(readback.equals)) {
          return { attempts, status: lastStatus, jsonPath: readback.jsonPath };
        }
      }
    } catch {
      // The outer deadline owns failure; transient readback errors are retried.
    }
    const remaining = timeoutMs - (Date.now() - started);
    if (remaining <= 0) break;
    await page.waitForTimeout(Math.min(readback.intervalMs, remaining));
  }
  throw new Error(`server readback did not reach the declared state (last status ${lastStatus ?? "unavailable"})`);
}

async function waitForRenderFrames(page, count) {
  await page.evaluate((frames) => new Promise((resolve) => {
    const advance = (remaining) => {
      if (remaining <= 0) resolve();
      else requestAnimationFrame(() => advance(remaining - 1));
    };
    advance(frames);
  }), count);
}

async function applyWaitFor(page, waitFor, armed = null) {
  const evidence = [];
  const timeout = waitFor.timeoutMs ?? 10000;
  const record = async (kind, operation, detail = {}) => {
    const started = Date.now();
    const value = await operation();
    evidence.push({ kind, durationMs: Date.now() - started, ...detail });
    return value;
  };
  const handles = armed || armWaitFor(page, waitFor);
  if (waitFor.responseUrl) {
    await record("response", async () => {
      const result = await handles.response;
      if (result.error) throw result.error;
      return result.value;
    }, { matcher: waitFor.responseUrl });
  }
  if (waitFor.url) {
    await record("url", async () => {
      const result = await handles.url;
      if (result.error) throw result.error;
      return result.value;
    }, { matcher: waitFor.url });
  }
  if (waitFor.selector && waitFor.errorSelector) {
    const outcome = await record("ready-or-error-dom", () => page.waitForFunction(
      ({ ready, error }) => {
        const visible = (selector) => {
          const element = document.querySelector(selector);
          if (!element) return false;
          const style = getComputedStyle(element);
          const rect = element.getBoundingClientRect();
          return style.display !== "none" && style.visibility !== "hidden" && rect.width > 0 && rect.height > 0;
        };
        if (visible(error)) return "error";
        if (visible(ready)) return "ready";
        return false;
      },
      { ready: waitFor.selector, error: waitFor.errorSelector },
      { timeout },
    ).then((handle) => handle.jsonValue()), {
      readySelector: waitFor.selector,
      errorSelector: waitFor.errorSelector,
    });
    if (outcome === "error") throw new Error(`error readiness selector became visible: ${waitFor.errorSelector}`);
  } else if (waitFor.selector) {
    await record("selector", () => page.waitForSelector(waitFor.selector, { timeout }), { selector: waitFor.selector });
  } else if (waitFor.errorSelector) {
    throw new Error("errorSelector requires selector");
  }
  if (waitFor.loadState) {
    await record("load-state", () => page.waitForLoadState(waitFor.loadState, {
      timeout: waitFor.loadStateTimeoutMs ?? timeout,
    }), { state: waitFor.loadState });
  } else if (waitFor.networkIdleMs !== undefined) {
    await record("network-idle", () => page.waitForLoadState("networkidle", {
      timeout: waitFor.networkIdleMs,
    }), { deadlineMs: waitFor.networkIdleMs });
  }
  if (waitFor.readback) {
    const readback = await record(
      "server-readback",
      () => waitForReadback(page, waitFor.readback, timeout),
      { url: waitFor.readback.url, expectedStatus: waitFor.readback.status },
    );
    evidence[evidence.length - 1].attempts = readback.attempts;
  }
  if (waitFor.renderFrames) {
    await record("render-frames", () => waitForRenderFrames(page, waitFor.renderFrames), {
      frames: waitFor.renderFrames,
    });
  } else if (!Object.keys(waitFor).length) {
    await record("render-frames", () => waitForRenderFrames(page, 2), { frames: 2 });
  }
  if (waitFor.settleMs) {
    await record("bounded-delay", () => page.waitForTimeout(waitFor.settleMs), { delayMs: waitFor.settleMs });
  }
  return evidence;
}

function normalizeActionList(value, name, { allowEmpty = false } = {}) {
  if (!Array.isArray(value) || (!allowEmpty && !value.length)) {
    throw new Error(`${name} must be ${allowEmpty ? "an" : "a non-empty"} array`);
  }
  return value.map((action, actionIndex) => {
    if (!action || typeof action !== "object" || Array.isArray(action)) {
      throw new Error(`${name}[${actionIndex}] must be an object`);
    }
    const kind = action.action;
    if (!["click", "hover", "focus", "fill", "check", "uncheck", "press", "selectOption"].includes(kind)) {
      throw new Error(`Unsupported declarative action: ${kind}`);
    }
    if (typeof action.selector !== "string" || !action.selector.trim()) {
      throw new Error(`${name}[${actionIndex}].selector must be non-empty`);
    }
    if (["fill", "press", "selectOption"].includes(kind) && action.value === undefined) {
      throw new Error(`Declarative ${kind} action requires value`);
    }
    if (["fill", "press"].includes(kind) && typeof action.value !== "string") {
      throw new Error(`Declarative ${kind} value must be a string`);
    }
    if (kind === "selectOption" && typeof action.value !== "string" && !Array.isArray(action.value)) {
      throw new Error("Declarative selectOption value must be a string or string array");
    }
    if (Array.isArray(action.value) && !action.value.every((item) => typeof item === "string")) {
      throw new Error("Declarative selectOption array values must all be strings");
    }
    const timeoutMs = action.timeoutMs === undefined ? 5000 : Number(action.timeoutMs);
    if (!Number.isFinite(timeoutMs) || timeoutMs < 0) {
      throw new Error("Declarative action timeoutMs must be a non-negative number");
    }
    const ownerJourney = action.ownerJourney === undefined ? null : action.ownerJourney;
    const ownerState = action.ownerState === undefined ? null : action.ownerState;
    if ((ownerJourney === null) !== (ownerState === null)) {
      throw new Error(`${name}[${actionIndex}] ownerJourney and ownerState must be supplied together`);
    }
    if (ownerJourney !== null && (
      typeof ownerJourney !== "string" || !ownerJourney.trim() ||
      typeof ownerState !== "string" || !ownerState.trim()
    )) {
      throw new Error(`${name}[${actionIndex}] ownerJourney and ownerState must be non-empty strings`);
    }
    return {
      action: kind,
      selector: action.selector.trim(),
      value: action.value,
      timeoutMs,
      ownerJourney: ownerJourney?.trim() || null,
      ownerState: ownerState?.trim() || null,
    };
  });
}

function normalizeExecution(value, name, base = null) {
  const fallback = base || {
    parallelSafe: false,
    resourceLocks: [],
    priority: null,
    stopOnFailure: false,
    stopReason: "",
  };
  if (value === undefined || value === null) return { ...fallback, resourceLocks: [...fallback.resourceLocks] };
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${name} must be an object`);
  }
  if (value.parallelSafe !== undefined && typeof value.parallelSafe !== "boolean") {
    throw new Error(`${name}.parallelSafe must be a boolean`);
  }
  if (value.stopOnFailure !== undefined && typeof value.stopOnFailure !== "boolean") {
    throw new Error(`${name}.stopOnFailure must be a boolean`);
  }
  const stopOnFailure = value.stopOnFailure ?? fallback.stopOnFailure;
  const stopReasonValue = value.stopReason === undefined ? fallback.stopReason : value.stopReason;
  if (stopReasonValue !== "" && (typeof stopReasonValue !== "string" || !stopReasonValue.trim())) {
    throw new Error(`${name}.stopReason must be a non-empty string when present`);
  }
  const stopReason = typeof stopReasonValue === "string" ? stopReasonValue.trim() : "";
  if (stopOnFailure && !stopReason) {
    throw new Error(`${name}.stopReason is required when stopOnFailure is true`);
  }
  const resourceLocks = value.resourceLocks === undefined ? fallback.resourceLocks : value.resourceLocks;
  if (!Array.isArray(resourceLocks) || !resourceLocks.every((item) => typeof item === "string" && item.trim())) {
    throw new Error(`${name}.resourceLocks must be an array of non-empty strings`);
  }
  const priorityValue = value.priority === undefined ? fallback.priority : value.priority;
  const priority = priorityValue === null ? null : Number(priorityValue);
  if (priority !== null && (!Number.isInteger(priority) || Math.abs(priority) > 100000)) {
    throw new Error(`${name}.priority must be an integer between -100000 and 100000`);
  }
  return {
    parallelSafe: value.parallelSafe ?? fallback.parallelSafe,
    resourceLocks: [...new Set(resourceLocks.map((item) => item.trim()))].sort(),
    priority,
    stopOnFailure,
    stopReason,
  };
}

function normalizeExecutionOverride(value, name) {
  if (value === undefined || value === null) return undefined;
  const validated = normalizeExecution(value, name);
  return {
    ...(Object.hasOwn(value, "parallelSafe") ? { parallelSafe: validated.parallelSafe } : {}),
    ...(Object.hasOwn(value, "resourceLocks") ? { resourceLocks: validated.resourceLocks } : {}),
    ...(Object.hasOwn(value, "priority") ? { priority: validated.priority } : {}),
    ...(Object.hasOwn(value, "stopOnFailure") ? { stopOnFailure: validated.stopOnFailure } : {}),
    ...(Object.hasOwn(value, "stopReason") ? { stopReason: validated.stopReason } : {}),
  };
}

function normalizeAuthProfileName(value, name) {
  if (value === undefined || value === null) return null;
  if (typeof value !== "string" || !value.trim()) throw new Error(`${name} must be a non-empty string`);
  return value.trim();
}

function normalizeAuthProfiles(value) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new Error("authProfiles must be an array");
  const seen = new Set();
  return value.map((profile, index) => {
    if (!profile || typeof profile !== "object" || Array.isArray(profile)) {
      throw new Error(`authProfiles[${index}] must be an object`);
    }
    if (typeof profile.name !== "string" || !profile.name.trim()) {
      throw new Error(`authProfiles[${index}].name must be non-empty`);
    }
    const name = profile.name.trim();
    if (seen.has(name)) throw new Error(`duplicate auth profile ${name}`);
    seen.add(name);
    if (typeof profile.url !== "string" || !profile.url.trim()) {
      throw new Error(`authProfiles[${index}].url must be non-empty`);
    }
    const actions = normalizeActionList(profile.actions, `authProfiles[${index}].actions`);
    if (actions.some((action) => action.ownerJourney || action.ownerState)) {
      throw new Error(`authProfiles[${index}].actions cannot declare conditional ownership`);
    }
    return {
      name,
      url: profile.url.trim(),
      actions,
      waitFor: normalizeWaitFor(profile.waitFor, `authProfiles[${index}].waitFor`),
    };
  });
}

function normalizeRenderedPerformanceOverride(value, name) {
  if (value === undefined || value === null) return {};
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${name} must be an object`);
  }
  const normalized = {};
  for (const key of ["ttfbMs", "lcpMs"]) {
    if (value[key] === undefined) continue;
    const threshold = Number(value[key]);
    if (!Number.isFinite(threshold) || threshold <= 0) {
      throw new Error(`${name}.${key} must be a positive number`);
    }
    normalized[key] = threshold;
  }
  if (value.ttfbLocalOnly !== undefined) {
    if (typeof value.ttfbLocalOnly !== "boolean") {
      throw new Error(`${name}.ttfbLocalOnly must be a boolean`);
    }
    normalized.ttfbLocalOnly = value.ttfbLocalOnly;
  }
  return normalized;
}

function resolveRenderedPerformance(...values) {
  return Object.assign({}, DEFAULT_RENDERED_PERFORMANCE, ...values.filter(Boolean));
}

function normalizeTargetDefaults(value) {
  if (value === undefined || value === null) return {};
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("targetDefaults must be an object");
  }
  return {
    journeys: normalizeJourneyDefinitions(value.journeys, "targetDefaults.journeys"),
    primaryJourney: value.primaryJourney === undefined || value.primaryJourney === null
      ? null
      : String(value.primaryJourney).trim(),
    priorityOverrideReason: value.priorityOverrideReason === undefined || value.priorityOverrideReason === null
      ? ""
      : String(value.priorityOverrideReason).trim(),
    regions: normalizeJourneyRegions(value.regions, "targetDefaults.regions"),
    theme: normalizeTheme(value.theme, "targetDefaults.theme"),
    reviewInputs: normalizeReviewInputs(value.reviewInputs, "targetDefaults.reviewInputs"),
    allowContrast: normalizeSelectorReasonList(value.allowContrast, "targetDefaults.allowContrast"),
    themeExceptions: normalizeSelectorReasonList(value.themeExceptions, "targetDefaults.themeExceptions"),
    screenshotMasks: normalizeSelectorReasonList(value.screenshotMasks, "targetDefaults.screenshotMasks"),
    execution: normalizeExecution(value.execution, "targetDefaults.execution"),
    authProfile: normalizeAuthProfileName(value.authProfile, "targetDefaults.authProfile"),
    performance: normalizeRenderedPerformanceOverride(
      value.performance,
      "targetDefaults.performance",
    ),
  };
}

function normalizeTargets(config, cli) {
  const targets = [];
  const targetDefaults = normalizeTargetDefaults(config.targetDefaults);
  const normalizeStates = (value) => {
    if (value === undefined) return [];
    if (!Array.isArray(value)) throw new Error("target.states must be an array");
    return value.map((state, stateIndex) => {
      if (!state || typeof state !== "object" || Array.isArray(state)) {
        throw new Error(`target.states[${stateIndex}] must be an object`);
      }
      if (typeof state.name !== "string" || !state.name.trim()) {
        throw new Error(`target.states[${stateIndex}].name must be a non-empty string`);
      }
      const actions = normalizeActionList(state.actions, `target.states[${stateIndex}].actions`);
      if (state.allowFailure !== undefined && (typeof state.allowFailure !== "string" || !state.allowFailure.trim())) {
        throw new Error(`target.states[${stateIndex}].allowFailure must be a non-empty reason string`);
      }
      const continuation = normalizeContinuation(
        state.continuation,
        `target.states[${stateIndex}].continuation`,
      );
      if (
        continuation?.triggerActionIndex !== null &&
        continuation?.triggerActionIndex >= actions.length
      ) {
        throw new Error(`target.states[${stateIndex}].continuation.triggerActionIndex is outside the actions array`);
      }
      return {
        name: state.name.trim(),
        actions,
        waitFor: normalizeWaitFor(state.waitFor, `target.states[${stateIndex}].waitFor`),
        afterFailureWaitFor: normalizeWaitFor(
          state.afterFailureWaitFor,
          `target.states[${stateIndex}].afterFailureWaitFor`,
        ),
        allowFailure: state.allowFailure,
        continuation,
        journeys: state.journeys === undefined
          ? undefined
          : normalizeJourneyDefinitions(state.journeys, `target.states[${stateIndex}].journeys`),
        primaryJourney: state.primaryJourney === undefined
          ? undefined
          : (state.primaryJourney === null ? null : String(state.primaryJourney).trim()),
        priorityOverrideReason: state.priorityOverrideReason === undefined
          ? undefined
          : (state.priorityOverrideReason === null ? "" : String(state.priorityOverrideReason).trim()),
        regions: state.regions === undefined
          ? undefined
          : normalizeJourneyRegions(state.regions, `target.states[${stateIndex}].regions`),
        theme: state.theme === undefined
          ? undefined
          : normalizeTheme(state.theme, `target.states[${stateIndex}].theme`),
        reviewInputs: state.reviewInputs === undefined
          ? undefined
          : normalizeReviewInputs(state.reviewInputs, `target.states[${stateIndex}].reviewInputs`),
        execution: state.execution === undefined
          ? undefined
          : normalizeExecutionOverride(state.execution, `target.states[${stateIndex}].execution`),
        authProfile: state.authProfile === undefined
          ? undefined
          : normalizeAuthProfileName(state.authProfile, `target.states[${stateIndex}].authProfile`),
        performance: state.performance === undefined
          ? undefined
          : normalizeRenderedPerformanceOverride(
              state.performance,
              `target.states[${stateIndex}].performance`,
            ),
      };
    });
  };
  if (Array.isArray(config.targets)) {
    for (const [targetIndex, item] of config.targets.entries()) {
      if (typeof item === "string") {
        targets.push({ ...targetDefaults, url: item, source: "explicit", targetGroupId: `target-${targetIndex + 1}` });
      }
      else if (item && typeof item === "object" && typeof item.url === "string") {
        if (item.allowFailure !== undefined && (typeof item.allowFailure !== "string" || !item.allowFailure.trim())) {
          throw new Error("target.allowFailure must be a non-empty reason string");
        }
        if (item.includeBase !== undefined && typeof item.includeBase !== "boolean") {
          throw new Error("target.includeBase must be a boolean");
        }
        targets.push({
          ...item,
          states: normalizeStates(item.states),
          contentInsets: normalizeContentInsetList(item.contentInsets, `targets[${targetIndex}].contentInsets`),
          journeys: item.journeys === undefined
            ? (targetDefaults.journeys || [])
            : normalizeJourneyDefinitions(item.journeys, `targets[${targetIndex}].journeys`),
          primaryJourney: item.primaryJourney === undefined
            ? (targetDefaults.primaryJourney || null)
            : (item.primaryJourney === null ? null : String(item.primaryJourney).trim()),
          priorityOverrideReason: item.priorityOverrideReason === undefined
            ? (targetDefaults.priorityOverrideReason || "")
            : (item.priorityOverrideReason === null ? "" : String(item.priorityOverrideReason).trim()),
          regions: item.regions === undefined
            ? (targetDefaults.regions || [])
            : normalizeJourneyRegions(item.regions, `targets[${targetIndex}].regions`),
          theme: item.theme === undefined
            ? (targetDefaults.theme || null)
            : normalizeTheme(item.theme, `targets[${targetIndex}].theme`),
          reviewInputs: item.reviewInputs === undefined
            ? (targetDefaults.reviewInputs || [])
            : normalizeReviewInputs(item.reviewInputs, `targets[${targetIndex}].reviewInputs`),
          allowContrast: [
            ...(targetDefaults.allowContrast || []),
            ...normalizeSelectorReasonList(item.allowContrast, `targets[${targetIndex}].allowContrast`),
          ],
          themeExceptions: [
            ...(targetDefaults.themeExceptions || []),
            ...normalizeSelectorReasonList(item.themeExceptions, `targets[${targetIndex}].themeExceptions`),
          ],
          screenshotMasks: [
            ...(targetDefaults.screenshotMasks || []),
            ...normalizeSelectorReasonList(item.screenshotMasks, `targets[${targetIndex}].screenshotMasks`),
          ],
          breakpointProfile: normalizeBreakpointProfile(item.breakpointProfile, `targets[${targetIndex}].breakpointProfile`),
          sourceBinding: item.sourceBinding === undefined
            ? undefined
            : normalizeSourceBinding(item.sourceBinding, `targets[${targetIndex}].sourceBinding`),
          waitFor: normalizeWaitFor(item.waitFor, `targets[${targetIndex}].waitFor`),
          execution: normalizeExecution(
            item.execution,
            `targets[${targetIndex}].execution`,
            targetDefaults.execution,
          ),
          authProfile: item.authProfile === undefined
            ? targetDefaults.authProfile
            : normalizeAuthProfileName(item.authProfile, `targets[${targetIndex}].authProfile`),
          performance: {
            ...(targetDefaults.performance || {}),
            ...normalizeRenderedPerformanceOverride(
              item.performance,
              `targets[${targetIndex}].performance`,
            ),
          },
          targetGroupId: `target-${targetIndex + 1}`,
          includeBase: item.includeBase === undefined ? true : item.includeBase,
          source: item.source || "explicit",
        });
      }
      else throw new Error("targets entries must be strings or objects with url");
    }
  }
  for (const [urlIndex, url] of cli.urls.entries()) {
    targets.push({
      ...targetDefaults,
      url,
      source: "explicit",
      targetGroupId: `cli-target-${urlIndex + 1}`,
    });
  }
  return targets;
}

function normalizeViewports(config, cli) {
  const viewports = [];
  if (Array.isArray(config.viewports)) {
    for (const item of config.viewports) {
      if (typeof item === "string") viewports.push(parseViewport(item));
      else if (item && typeof item === "object" && (item.device || (item.width && item.height))) {
        const normalized = {
          ...item,
          name: String(item.name || item.device || "viewport"),
          device: item.device === undefined ? undefined : String(item.device),
          width: item.width === undefined ? undefined : Number(item.width),
          height: item.height === undefined ? undefined : Number(item.height),
        };
        if (normalized.width !== undefined && (!Number.isFinite(normalized.width) || normalized.width <= 0)) {
          throw new Error("viewport width must be a positive number");
        }
        if (normalized.height !== undefined && (!Number.isFinite(normalized.height) || normalized.height <= 0)) {
          throw new Error("viewport height must be a positive number");
        }
        viewports.push(normalized);
      } else {
        throw new Error("viewports entries must be name=WIDTHxHEIGHT strings, {name,width,height}, or {name,device}");
      }
    }
  }
  viewports.push(...cli.viewports);
  return viewports.length ? viewports : DEFAULT_VIEWPORTS;
}

function normalizeChangedPaths(value) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new Error("development.changedPaths must be an array");
  return [...new Set(value.map((item, index) => {
    if (typeof item !== "string" || !item.trim()) {
      throw new Error(`development.changedPaths[${index}] must be a non-empty repository-relative path`);
    }
    const normalized = item.trim().replaceAll("\\", "/").replace(/^\.\//, "");
    if (path.posix.isAbsolute(normalized) || normalized.split("/").includes("..")) {
      throw new Error(`development.changedPaths[${index}] must stay repository-relative`);
    }
    return normalized;
  }))].sort();
}

function normalizeDevelopment(config, cli, repoRoot) {
  const value = config.development === undefined ? {} : config.development;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("development must be an object");
  }
  const configuredChanged = normalizeChangedPaths(value.changedPaths);
  const changedPaths = normalizeChangedPaths([...configuredChanged, ...cli.changedPaths]);
  const cacheValue = value.cache === undefined ? {} : value.cache;
  if (!cacheValue || typeof cacheValue !== "object" || Array.isArray(cacheValue)) {
    throw new Error("development.cache must be an object");
  }
  const cacheDirectoryValue = cli.cacheDir ?? cacheValue.directory;
  const dataRevisionValue = cli.dataRevision ?? cacheValue.dataRevision;
  let cache = null;
  if (cacheDirectoryValue !== undefined) {
    if (typeof cacheDirectoryValue !== "string" || !cacheDirectoryValue.trim() || !path.isAbsolute(cacheDirectoryValue)) {
      throw new Error("development cache directory must be an explicit absolute path");
    }
    if (typeof dataRevisionValue !== "string" || !dataRevisionValue.trim()) {
      throw new Error("development cache requires a non-empty dataRevision");
    }
    const mode = cacheValue.mode === undefined ? "read-write" : cacheValue.mode;
    if (!["read", "write", "read-write"].includes(mode)) {
      throw new Error("development.cache.mode must be read, write, or read-write");
    }
    const directory = path.resolve(cacheDirectoryValue);
    if (repoRoot && pathIsWithin(directory, path.resolve(repoRoot))) {
      throw new Error("development cache directory must stay outside repoRoot");
    }
    cache = { directory, dataRevision: dataRevisionValue.trim(), mode };
  } else if (dataRevisionValue !== undefined) {
    throw new Error("dataRevision requires a development cache directory");
  }
  return {
    enabled: Boolean(changedPaths.length || cache),
    changedPaths,
    cache,
  };
}
function normalizeRequiredCoverage(value) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new Error("requiredCoverage must be an array");
  const seen = new Set();
  return value.map((entry, index) => {
    if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
      throw new Error(`requiredCoverage[${index}] must be an object`);
    }
    const allowed = new Set(["target", "state", "viewport", "width"]);
    const unknown = Object.keys(entry).filter((key) => !allowed.has(key));
    if (unknown.length) {
      throw new Error(`requiredCoverage[${index}] has unsupported fields: ${unknown.join(", ")}`);
    }
    const normalized = {};
    for (const key of ["target", "state", "viewport"]) {
      if (typeof entry[key] !== "string" || !entry[key].trim()) {
        throw new Error(`requiredCoverage[${index}].${key} must be a non-empty string`);
      }
      normalized[key] = entry[key].trim();
    }
    if (entry.width !== undefined) {
      const width = Number(entry.width);
      if (!Number.isInteger(width) || width <= 0) {
        throw new Error(`requiredCoverage[${index}].width must be a positive integer`);
      }
      normalized.width = width;
    }
    const signature = stableJson(normalized);
    if (seen.has(signature)) {
      throw new Error(`requiredCoverage[${index}] duplicates an earlier requirement`);
    }
    seen.add(signature);
    return {
      requirementId: `required-${String(index + 1).padStart(4, "0")}`,
      ...normalized,
    };
  });
}

function normalizeConfig(config, cli, artifacts) {
  const rules = config.rules && typeof config.rules === "object" ? config.rules : {};
  const failOn = cli.failOn || rules.failOn || "critical";
  if (!(failOn in SEVERITY_ORDER)) throw new Error(`Invalid failOn severity: ${failOn}`);
  const minCheckedPages = config.minCheckedPages === undefined ? 1 : Number(config.minCheckedPages);
  if (!Number.isInteger(minCheckedPages) || minCheckedPages < 0) {
    throw new Error("minCheckedPages must be a non-negative integer");
  }
  const maxPageCount = Number(cli.maxPageCount ?? config.maxPageCount ?? DEFAULT_MAX_PAGE_COUNT);
  if (!Number.isInteger(maxPageCount) || maxPageCount <= 0) {
    throw new Error("maxPageCount must be a positive integer");
  }
  const executionValue = config.execution === undefined ? {} : config.execution;
  if (!executionValue || typeof executionValue !== "object" || Array.isArray(executionValue)) {
    throw new Error("execution must be an object");
  }
  const maxConcurrency = Number(cli.concurrency ?? executionValue.maxConcurrency ?? DEFAULT_MAX_CONCURRENCY);
  if (!Number.isInteger(maxConcurrency) || maxConcurrency <= 0 || maxConcurrency > 32) {
    throw new Error("execution.maxConcurrency must be an integer from 1 through 32");
  }
  const areas = [
    ...(Array.isArray(config.areas) ? config.areas : []),
    ...cli.areas,
  ].map((item) => {
    if (!item || typeof item.name !== "string" || typeof item.selector !== "string") {
      throw new Error("areas entries must be {name, selector}");
    }
    return { name: item.name, selector: item.selector };
  });
  if (config.humanReadableStdout !== undefined) {
    throw new Error("humanReadableStdout is CLI-only; use --human-readable-stdout in an attended human terminal");
  }
  if (config.receiptOnly !== undefined && typeof config.receiptOnly !== "boolean") {
    throw new Error("receiptOnly must be a boolean when present");
  }
  const playwrightModuleDirValue = cli.playwrightModuleDir || config.playwrightModuleDir;
  if (playwrightModuleDirValue !== undefined && (
    typeof playwrightModuleDirValue !== "string" || !playwrightModuleDirValue.trim()
  )) {
    throw new Error("playwrightModuleDir must be a non-empty path string");
  }
  const artifactFiles = [
    artifacts.jsonOut,
    artifacts.markdownOut,
    artifacts.reviewQueueOut,
    artifacts.journeyEvidenceOut,
    artifacts.progressOut,
  ];
  if (new Set(artifactFiles).size !== artifactFiles.length) {
    throw new Error("JSON, Markdown, and review-queue artifact paths must be distinct");
  }
  if (artifactFiles.includes(artifacts.screenshotDir)) {
    throw new Error("screenshotDir must be distinct from report and review-queue files");
  }
  return {
    targets: normalizeTargets(config, cli),
    targetDefaults: normalizeTargetDefaults(config.targetDefaults),
    viewports: normalizeViewports(config, cli),
    waitFor: normalizeWaitFor(config.waitFor, "waitFor"),
    areas,
    contentInsets: normalizeContentInsetList(config.contentInsets, "contentInsets"),
    ignore: [...normalizeSelectorReasonList(config.ignore, "ignore"), ...cli.ignore],
    allowTruncation: [...normalizeSelectorReasonList(config.allowTruncation, "allowTruncation"), ...cli.allowTruncation],
    allowOverlap: [...normalizeSelectorReasonList(config.allowOverlap, "allowOverlap"), ...cli.allowOverlap],
    allowContrast: normalizeSelectorReasonList(config.allowContrast, "allowContrast"),
    themeExceptions: normalizeSelectorReasonList(config.themeExceptions, "themeExceptions"),
    screenshotMasks: normalizeSelectorReasonList(config.screenshotMasks, "screenshotMasks"),
    rules: {
      failOn,
      strictTruncation: Boolean(rules.strictTruncation),
    },
    jsonOut: artifacts.jsonOut,
    markdownOut: artifacts.markdownOut,
    reviewQueueOut: artifacts.reviewQueueOut,
    journeyEvidenceOut: artifacts.journeyEvidenceOut,
    progressOut: artifacts.progressOut,
    humanReadableStdout: cli.humanReadableStdout,
    browserExecutable: cli.browserExecutable || config.browserExecutable,
    playwrightModuleDir: playwrightModuleDirValue
      ? path.resolve(playwrightModuleDirValue)
      : undefined,
    fromCoordinator: cli.fromCoordinator || Boolean(config.fromCoordinator),
    coordinatorCommand: cli.coordinatorCommand || config.coordinatorCommand || "devcoordinator2",
    coordinatorProject: cli.coordinatorProject || config.coordinatorProject,
    onlyCurrent: cli.onlyCurrent || Boolean(config.onlyCurrent),
    allowDiscoveredTargetFailures:
      cli.allowDiscoveredTargetFailures || Boolean(config.allowDiscoveredTargetFailures),
    minCheckedPages,
    maxPageCount,
    execution: { maxConcurrency },
    performance: resolveRenderedPerformance(
      normalizeRenderedPerformanceOverride(config.performance, "performance"),
    ),
    screenshotDir: artifacts.screenshotDir,
    scroll: cli.noScroll ? false : (config.scroll === undefined ? true : Boolean(config.scroll)),
    cookies: [...normalizeCookieList(config.cookies), ...cli.cookies],
    ignoreHttpsErrors: cli.ignoreHttpsErrors || Boolean(config.ignoreHttpsErrors),
    sourceBinding: normalizeSourceBinding(config.sourceBinding, "sourceBinding"),
    repoRoot: cli.repoRoot || config.repoRoot || null,
    development: normalizeDevelopment(config, cli, cli.repoRoot || config.repoRoot || null),
    authProfiles: normalizeAuthProfiles(config.authProfiles),
    priorReview: loadPriorManualReview(cli.reviewAgainst || config.reviewAgainst),
    reviewRemovedCells: normalizeRemovedReviewCells(config.reviewRemovedCells),
    requiredCoverage: normalizeRequiredCoverage(config.requiredCoverage),
  };
}

function resolveViewports(viewports, devices) {
  return viewports.map((viewport) => {
    let contextOptions = {};
    if (viewport.device) {
      const descriptor = devices[viewport.device];
      if (!descriptor) throw new Error(`Unknown Playwright device descriptor: ${viewport.device}`);
      contextOptions = { ...descriptor };
      delete contextOptions.defaultBrowserType;
    }
    const descriptorViewport = contextOptions.viewport || {};
    const width = viewport.width ?? descriptorViewport.width;
    const height = viewport.height ?? descriptorViewport.height;
    if (!Number.isFinite(width) || width <= 0 || !Number.isFinite(height) || height <= 0) {
      throw new Error(`Viewport ${viewport.name} does not resolve to positive width and height`);
    }
    contextOptions.viewport = { width, height };
    const allowedContextOverrides = [
      "userAgent", "deviceScaleFactor", "isMobile", "hasTouch", "locale",
      "colorScheme", "reducedMotion", "forcedColors", "screen",
    ];
    for (const key of allowedContextOverrides) {
      if (viewport[key] !== undefined) contextOptions[key] = viewport[key];
    }
    return {
      name: viewport.name,
      device: viewport.device,
      width,
      height,
      contextOptions,
      sampling: {
        mode: "sampled-only",
        sources: ["configured"],
        breakpointSamples: [],
      },
    };
  });
}

function stableJson(value) {
  const normalize = (entry) => {
    if (Array.isArray(entry)) return entry.map(normalize);
    if (!entry || typeof entry !== "object") return entry;
    return Object.fromEntries(
      Object.keys(entry).sort().map((key) => [key, normalize(entry[key])]),
    );
  };
  return JSON.stringify(normalize(value));
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function privacySafeConfigContract(config) {
  const redactWaitFor = (waitFor) => waitFor?.readback && Object.hasOwn(waitFor.readback, "equals")
    ? { ...waitFor, readback: { ...waitFor.readback, equals: "<redacted>" } }
    : waitFor;
  const redactActions = (states) => (states || []).map((state) => ({
    ...state,
    waitFor: redactWaitFor(state.waitFor),
    afterFailureWaitFor: redactWaitFor(state.afterFailureWaitFor),
    actions: (state.actions || []).map((action) => ({
      ...action,
      ...(Object.hasOwn(action, "value") ? { value: "<redacted>" } : {}),
    })),
  }));
  const targets = (config.targets || []).map((target) => ({
    ...target,
    waitFor: redactWaitFor(target.waitFor),
    states: redactActions(target.states),
  }));
  const cookies = (config.cookies || []).map((cookie) => ({
    ...cookie,
    value: "<redacted>",
  }));
  return {
    targets,
    targetDefaults: config.targetDefaults,
    viewports: config.viewports,
    waitFor: config.waitFor || null,
    areas: config.areas,
    contentInsets: config.contentInsets,
    ignore: config.ignore,
    allowTruncation: config.allowTruncation,
    allowOverlap: config.allowOverlap,
    allowContrast: config.allowContrast,
    themeExceptions: config.themeExceptions,
    screenshotMasks: config.screenshotMasks,
    rules: config.rules,
    fromCoordinator: config.fromCoordinator,
    coordinatorProject: config.coordinatorProject || null,
    onlyCurrent: config.onlyCurrent,
    allowDiscoveredTargetFailures: config.allowDiscoveredTargetFailures,
    minCheckedPages: config.minCheckedPages,
    maxPageCount: config.maxPageCount,
    scroll: config.scroll,
    cookies,
    authProfiles: (config.authProfiles || []).map((profile) => ({
      ...profile,
      waitFor: redactWaitFor(profile.waitFor),
      actions: (profile.actions || []).map((action) => ({
        ...action,
        ...(Object.hasOwn(action, "value") ? { value: "<redacted>" } : {}),
      })),
    })),
    execution: config.execution,
    performance: config.performance,
    development: config.development,
    ignoreHttpsErrors: config.ignoreHttpsErrors,
    sourceBinding: config.sourceBinding,
    repoRoot: config.repoRoot ? path.resolve(config.repoRoot) : null,
    priorReview: config.priorReview
      ? { sha256: config.priorReview.sha256, reviewedRunId: config.priorReview.reviewedRunId }
      : null,
    reviewRemovedCells: config.reviewRemovedCells,
    requiredCoverage: config.requiredCoverage,
    browserExecutable: config.browserExecutable || null,
    playwrightModuleDir: config.playwrightModuleDir || null,
  };
}

function routeEvidence(value) {
  try {
    const parsed = new URL(value);
    return {
      origin: parsed.origin,
      path: parsed.pathname || "/",
      queryPresent: Boolean(parsed.search),
      fragmentPresent: Boolean(parsed.hash),
    };
  } catch {
    return {
      origin: "",
      path: String(value || ""),
      queryPresent: false,
      fragmentPresent: false,
    };
  }
}

function viewportExecutionSignature(viewport) {
  return stableJson({
    width: viewport.width,
    height: viewport.height,
    contextOptions: viewport.contextOptions,
  });
}

function viewportsForTarget(target, configuredViewports) {
  const viewports = configuredViewports.map((viewport) => ({
    ...viewport,
    contextOptions: { ...viewport.contextOptions },
    sampling: {
      mode: "sampled-only",
      sources: [...(viewport.sampling?.sources || ["configured"])],
      breakpointSamples: [...(viewport.sampling?.breakpointSamples || [])],
    },
  }));
  const profile = target.breakpointProfile;
  if (!profile) return viewports;
  let base = null;
  if (profile.baseViewport) {
    base = configuredViewports.find((viewport) => viewport.name === profile.baseViewport);
    if (!base) {
      throw new Error(
        `Breakpoint profile ${profile.name} references unknown baseViewport ${profile.baseViewport}`,
      );
    }
  }
  const bySignature = new Map(viewports.map((viewport) => [viewportExecutionSignature(viewport), viewport]));
  for (const breakpoint of profile.breakpoints) {
    for (const offset of [-1, 0, 1]) {
      const width = breakpoint + offset;
      const contextOptions = base
        ? { ...base.contextOptions, viewport: { width, height: profile.height } }
        : { viewport: { width, height: profile.height } };
      const sample = {
        profile: profile.name,
        breakpoint,
        offset,
      };
      const candidate = {
        name: `${profile.name}-${breakpoint}-${offset < 0 ? "minus-1" : (offset > 0 ? "plus-1" : "at")}`,
        device: base?.device,
        width,
        height: profile.height,
        contextOptions,
        sampling: {
          mode: "sampled-only",
          sources: ["breakpoint-profile"],
          breakpointSamples: [sample],
        },
      };
      const signature = viewportExecutionSignature(candidate);
      const existing = bySignature.get(signature);
      if (existing) {
        if (!existing.sampling.sources.includes("breakpoint-profile")) {
          existing.sampling.sources.push("breakpoint-profile");
        }
        existing.sampling.breakpointSamples.push(sample);
      } else {
        viewports.push(candidate);
        bySignature.set(signature, candidate);
      }
    }
  }
  return viewports;
}

function executionPriorityForTarget(target) {
  if (target.execution?.priority !== null && target.execution?.priority !== undefined) {
    return target.execution.priority;
  }
  const journey = (target.journeys || []).find((item) => item.id === target.primaryJourney);
  const riskWeight = { critical: 40000, high: 30000, normal: 20000, low: 10000 };
  return (riskWeight[journey?.risk] || riskWeight.normal) + (journey?.frequencyPercent || 0);
}

function buildExecutionPlan(targets, configuredViewports, maxPageCount) {
  const cells = [];
  for (const target of targets) {
    for (const viewport of viewportsForTarget(target, configuredViewports)) {
      const cellId = `cell-${String(cells.length + 1).padStart(4, "0")}`;
      const requestedRoute = routeEvidence(target.url);
      cells.push({
        cellId,
        planIndex: cells.length,
        executionPriority: executionPriorityForTarget(target),
        target,
        viewport,
        requestedPath: requestedRoute.path,
      });
      if (cells.length > maxPageCount) {
        throw new Error(
          `Expanded verification plan has at least ${cells.length} page cells, exceeding maxPageCount ${maxPageCount}`,
        );
      }
    }
  }
  return cells;
}
function evaluateRequiredCoverage(fullPlan, requirements) {
  const entries = requirements.map((requirement) => {
    const matches = fullPlan.filter((cell) =>
      cell.target.baseTargetName === requirement.target &&
      (cell.target.stateName || "base") === requirement.state &&
      cell.viewport.name === requirement.viewport &&
      (requirement.width === undefined || cell.viewport.width === requirement.width)
    );
    const status = matches.length === 1 ? "satisfied" : (matches.length ? "ambiguous" : "missing");
    const expected = `${requirement.target} [${requirement.state}; ${requirement.viewport}${requirement.width === undefined ? "" : ` @ ${requirement.width}px`}]`;
    return {
      ...requirement,
      status,
      matchingCellIds: matches.map((cell) => cell.cellId),
      reason: status === "satisfied"
        ? ""
        : (status === "missing"
          ? `required verification cell is not present: ${expected}`
          : `required verification cell matches ${matches.length} plan cells and is ambiguous: ${expected}`),
    };
  });
  return {
    declaredCount: entries.length,
    satisfiedCount: entries.filter((entry) => entry.status === "satisfied").length,
    failed: entries.some((entry) => entry.status !== "satisfied"),
    entries,
  };
}

function changedPathMatchesFile(changedPath, filePath) {
  return changedPath === filePath ||
    filePath.startsWith(`${changedPath}/`) ||
    changedPath.startsWith(`${filePath}/`);
}

function selectExecutionCells(fullPlan, development) {
  const base = {
    mode: development.enabled ? "development" : "complete",
    readinessEligible: !development.enabled,
    changedPaths: development.changedPaths,
    fullPlanCount: fullPlan.length,
    selectedCount: fullPlan.length,
    fallbackToFull: false,
    unmappedPaths: [],
    selectedTargetGroups: [...new Set(fullPlan.map((cell) => cell.target.targetGroupId))],
  };
  if (!development.changedPaths.length) {
    return { cells: fullPlan, selection: base };
  }
  const matchedGroups = new Set();
  const unmappedPaths = [];
  for (const changedPath of development.changedPaths) {
    const groups = new Set();
    for (const cell of fullPlan) {
      const files = cell.target.reviewEvidence?.files || [];
      if (files.some((file) => changedPathMatchesFile(changedPath, file.path))) {
        groups.add(cell.target.targetGroupId);
      }
    }
    if (!groups.size) unmappedPaths.push(changedPath);
    for (const group of groups) matchedGroups.add(group);
  }
  if (unmappedPaths.length || !matchedGroups.size) {
    return {
      cells: fullPlan,
      selection: {
        ...base,
        fallbackToFull: true,
        unmappedPaths,
      },
    };
  }
  const cells = fullPlan.filter((cell) => matchedGroups.has(cell.target.targetGroupId));
  return {
    cells,
    selection: {
      ...base,
      selectedCount: cells.length,
      selectedTargetGroups: [...matchedGroups].sort(),
    },
  };
}

function publicExecutionPlan(cells, maxPageCount, selection = null, requiredCoverage = null) {
  return {
    pageBudget: maxPageCount,
    plannedPageCount: cells.length,
    fullDeclaredPageCount: selection?.fullPlanCount ?? cells.length,
    selection: selection || {
      mode: "complete",
      readinessEligible: true,
      fullPlanCount: cells.length,
      selectedCount: cells.length,
    },
    requiredCoverage,
    widthCoverage: "sampled-only",
    widthCoverageNote: "Only the listed viewport widths were checked; widths between samples were not inspected.",
    cells: cells.map((cell) => ({
      cellId: cell.cellId,
      planIndex: cell.planIndex,
      executionPriority: cell.executionPriority,
      targetName: cell.target.name || cell.target.url,
      requestedPath: cell.requestedPath,
      stateName: cell.target.stateName || "base",
      viewport: publicViewport(cell.viewport),
    })),
  };
}

function publicTarget(target) {
  const { states, verificationState, includeBase, repositoryRoot, targetGroupId, baseTargetName, ...safe } = target;
  return safe;
}

function publicViewport(viewport) {
  const { contextOptions, ...safe } = viewport;
  return safe;
}

function expandTargetStates(targets) {
  const expanded = [];
  for (const target of targets) {
    const states = Array.isArray(target.states) ? target.states : [];
    const baseName = target.name || target.url;
    if (target.includeBase !== false || !states.length) {
      expanded.push({
        ...target,
        name: baseName,
        baseTargetName: baseName,
        stateName: "base",
        continuation: null,
        execution: normalizeExecution(target.execution, "target.execution"),
      });
    }
    for (const state of states) {
      expanded.push({
        ...target,
        name: `${baseName} [${state.name}]`,
        baseTargetName: baseName,
        stateName: state.name,
        verificationState: state,
        allowFailure: state.allowFailure || target.allowFailure,
        journeys: state.journeys ?? target.journeys,
        primaryJourney: state.primaryJourney ?? target.primaryJourney,
        priorityOverrideReason: state.priorityOverrideReason ?? target.priorityOverrideReason,
        regions: state.regions ?? target.regions,
        theme: state.theme ?? target.theme,
        reviewInputs: [
          ...(target.reviewInputs || []),
          ...(state.reviewInputs || []),
        ],
        continuation: state.continuation,
        afterFailureWaitFor: state.afterFailureWaitFor,
        execution: normalizeExecution(state.execution, `state ${state.name}.execution`, target.execution),
        authProfile: state.authProfile ?? target.authProfile,
        performance: {
          ...(target.performance || {}),
          ...(state.performance || {}),
        },
      });
    }
  }
  return expanded;
}

function pathHasSymlinkComponent(absolutePath) {
  const parsed = path.parse(absolutePath);
  let current = parsed.root;
  const relative = absolutePath.slice(parsed.root.length);
  for (const part of relative.split(path.sep).filter(Boolean)) {
    current = path.join(current, part);
    if (fs.lstatSync(current).isSymbolicLink()) return true;
  }
  return false;
}

function resolveRepositoryRoot(value) {
  if (value === null || value === undefined || value === "") {
    return { root: null, error: "repoRoot is required for declared UI review inputs" };
  }
  if (typeof value !== "string" || !value.trim()) {
    return { root: null, error: "repoRoot must be a non-empty path string" };
  }
  const candidate = path.resolve(value);
  try {
    if (pathHasSymlinkComponent(candidate)) {
      return { root: null, error: "repoRoot must not contain symlinked path components" };
    }
    const stat = fs.lstatSync(candidate);
    if (!stat.isDirectory() || stat.isSymbolicLink()) {
      return { root: null, error: "repoRoot must be a regular non-symlink directory" };
    }
    return { root: fs.realpathSync.native(candidate), error: null };
  } catch (error) {
    return { root: null, error: `repoRoot could not be resolved: ${error.message}` };
  }
}

function collectReviewInputFiles(repoRoot, declaredInputs) {
  const files = new Map();
  const inputs = [];
  const walk = (absolute, kind) => {
    const stat = fs.lstatSync(absolute);
    if (stat.isSymbolicLink()) throw new Error("symlinked review inputs are not allowed");
    if (stat.isFile()) {
      const relativePath = path.relative(repoRoot, absolute).split(path.sep).join("/");
      const bytes = fs.readFileSync(absolute);
      const existing = files.get(relativePath);
      const kinds = new Set(existing?.kinds || []);
      kinds.add(kind);
      files.set(relativePath, {
        path: relativePath,
        sha256: sha256(bytes),
        kinds: [...kinds].sort(),
      });
      return 1;
    }
    if (!stat.isDirectory()) throw new Error("review inputs must be regular files or directories");
    let discovered = 0;
    for (const entry of fs.readdirSync(absolute, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const child = path.join(absolute, entry.name);
      if (entry.isSymbolicLink()) throw new Error(`symlinked review input is not allowed: ${entry.name}`);
      discovered += walk(child, kind);
    }
    return discovered;
  };
  for (const input of declaredInputs) {
    if (path.isAbsolute(input.path) || input.path.split(/[\\/]+/).includes("..")) {
      throw new Error(`review input must stay repository-relative: ${input.path}`);
    }
    const absolute = path.resolve(repoRoot, input.path);
    if (!pathIsWithin(absolute, repoRoot)) {
      throw new Error(`review input escapes repoRoot: ${input.path}`);
    }
    if (!fs.existsSync(absolute)) throw new Error(`review input does not exist: ${input.path}`);
    if (pathHasSymlinkComponent(absolute)) {
      throw new Error(`review input contains a symlinked path component: ${input.path}`);
    }
    const discovered = walk(absolute, input.kind);
    if (discovered === 0 && fs.lstatSync(absolute).isDirectory()) {
      throw new Error(`review input directory is empty: ${input.path}`);
    }
    inputs.push({ path: input.path.split(path.sep).join("/"), kind: input.kind });
  }
  const fileList = [...files.values()].sort((a, b) => a.path.localeCompare(b.path));
  if (!fileList.length) throw new Error("reviewInputs resolved no files");
  return {
    inputs,
    files: fileList,
    fingerprint: sha256(stableJson({ inputs, files: fileList })),
  };
}

function journeyContractErrors(target) {
  const errors = [];
  const journeys = Array.isArray(target.journeys) ? target.journeys : [];
  const journeyIds = new Set(journeys.map((journey) => journey.id));
  if (!journeys.length) errors.push("journeys must declare at least one journey");
  if (!target.primaryJourney) errors.push("primaryJourney is required");
  else if (!journeyIds.has(target.primaryJourney)) errors.push("primaryJourney must reference a declared journey id");
  if (journeys.length && target.primaryJourney && journeyIds.has(target.primaryJourney)) {
    const primary = journeys.find((journey) => journey.id === target.primaryJourney);
    const highestFrequency = Math.max(...journeys.map((journey) => journey.frequencyPercent));
    if (primary.frequencyPercent < highestFrequency && !String(target.priorityOverrideReason || "").trim()) {
      errors.push("a lower-frequency primaryJourney requires priorityOverrideReason");
    }
  }
  const regions = Array.isArray(target.regions) ? target.regions : [];
  if (!regions.length) errors.push("regions must declare the rendered journey hierarchy");
  const primaryRegions = regions.filter(
    (region) => region.role === "primary-content" && region.journey === target.primaryJourney,
  );
  if (!primaryRegions.length) errors.push("regions must include primary-content for primaryJourney");
  for (const region of regions) {
    if (["primary-content", "workflow-surface"].includes(region.role)) {
      if (!region.journey || !journeyIds.has(region.journey)) {
        errors.push(`region ${region.name} must reference a declared journey`);
      }
    }
    if (region.role === "blocking-alert" && !region.reason) {
      errors.push(`blocking-alert region ${region.name} requires a reason`);
    }
  }
  if (!target.theme) errors.push("theme must declare light, dark, or mixed");
  if (!Array.isArray(target.reviewInputs) || !target.reviewInputs.length) {
    errors.push("reviewInputs must declare the UI implementation inputs for this target/state");
  }
  const actions = target.verificationState?.actions || [];
  const activating = actions.some((action) =>
    ["click", "press", "check", "uncheck", "selectOption"].includes(action.action) &&
    (!action.ownerState || action.ownerState === target.stateName)
  );
  if (activating && !target.continuation) {
    errors.push("an activating interaction state requires a continuation checkpoint");
  }
  return [...new Set(errors)];
}

function prepareTargetContracts(targets, config) {
  const repo = resolveRepositoryRoot(config.repoRoot);
  const reviewCache = new Map();
  const prepared = targets.map((target) => {
    const contractErrors = journeyContractErrors(target);
    let reviewEvidence = null;
    if (target.reviewInputs?.length) {
      if (repo.error) {
        contractErrors.push(repo.error);
      } else {
        const cacheKey = stableJson(target.reviewInputs);
        try {
          if (!reviewCache.has(cacheKey)) {
            reviewCache.set(cacheKey, collectReviewInputFiles(repo.root, target.reviewInputs));
          }
          reviewEvidence = reviewCache.get(cacheKey);
        } catch (error) {
          contractErrors.push(error.message);
        }
      }
    }
    const intentContract = {
      journeys: target.journeys || [],
      primaryJourney: target.primaryJourney || null,
      priorityOverrideReason: target.priorityOverrideReason || "",
      regions: target.regions || [],
      theme: target.theme || null,
      stateName: target.stateName || "base",
      continuation: target.continuation || null,
      actions: (target.verificationState?.actions || []).map((action) => ({
        action: action.action,
        selector: action.selector,
        timeoutMs: action.timeoutMs,
        ownerJourney: action.ownerJourney,
        ownerState: action.ownerState,
        ...(Object.hasOwn(action, "value") ? { value: "<redacted>" } : {}),
      })),
      afterFailureWaitFor: target.afterFailureWaitFor || null,
      execution: target.execution,
      authProfile: target.authProfile || null,
    };
    return {
      ...target,
      performance: resolveRenderedPerformance(config.performance, target.performance),
      contractErrors: [...new Set(contractErrors)],
      reviewEvidence,
      intentFingerprint: sha256(stableJson(intentContract)),
      repositoryRoot: repo.root,
    };
  });
  const authNames = new Set(config.authProfiles.map((profile) => profile.name));
  for (const target of prepared) {
    if (target.authProfile && !authNames.has(target.authProfile)) {
      target.contractErrors.push(`authProfile references unknown profile ${target.authProfile}`);
    }
    const journeyIds = new Set((target.journeys || []).map((journey) => journey.id));
    for (const action of target.verificationState?.actions || []) {
      if (!action.ownerJourney) continue;
      if (!journeyIds.has(action.ownerJourney)) {
        target.contractErrors.push(`conditional action ownerJourney ${action.ownerJourney} is not declared`);
      }
      const owners = prepared.filter((candidate) =>
        candidate.targetGroupId === target.targetGroupId &&
        candidate.stateName === action.ownerState &&
        candidate.primaryJourney === action.ownerJourney
      );
      if (owners.length !== 1) {
        target.contractErrors.push(
          `conditional action owner ${action.ownerJourney}/${action.ownerState} must resolve to exactly one state`,
        );
      }
    }
    target.contractErrors = [...new Set(target.contractErrors)];
  }
  return prepared;
}

async function applyInteractionState(page, state, target = null) {
  if (!state) return { beforeContinuation: null, waitEvidence: [], actionTimings: [], handoff: null, failure: null };
  const triggerActionIndex = state.continuation
    ? (state.continuation.triggerActionIndex ?? state.actions.length - 1)
    : state.actions.length - 1;
  let beforeContinuation = null;
  const actionTimings = [];
  const waitEvidence = [];
  let armed = null;
  const failureArmed = Object.keys(state.afterFailureWaitFor || {}).length
    ? armWaitFor(page, state.afterFailureWaitFor)
    : null;
  for (const [index, action] of state.actions.entries()) {
    const locator = page.locator(action.selector);
    const options = { timeout: action.timeoutMs };
    const actionStarted = Date.now();
    try {
      const count = await locator.count();
      const controlVisible = count > 0 && await locator.first().isVisible();
      const isOwnerState = Boolean(
        action.ownerState && target &&
        action.ownerState === target.stateName &&
        action.ownerJourney === target.primaryJourney,
      );
      if (action.ownerState && !isOwnerState && controlVisible) {
        actionTimings.push({
          index,
          action: action.action,
          selector: action.selector,
          durationMs: Date.now() - actionStarted,
          outcome: "ownership-contradiction",
        });
        return {
          beforeContinuation,
          waitEvidence,
          actionTimings,
          handoff: null,
          failure: {
            kind: "conditional-ownership-contradiction",
            message: `${action.selector} is visible outside its declared owner ${action.ownerJourney}/${action.ownerState}`,
          },
        };
      }
      if (action.ownerState && !isOwnerState && !controlVisible) {
          actionTimings.push({
            index,
            action: action.action,
            selector: action.selector,
            durationMs: Date.now() - actionStarted,
            outcome: "handed-off",
          });
          return {
            beforeContinuation,
            waitEvidence,
            actionTimings,
            failure: null,
            handoff: {
              selector: action.selector,
              ownerJourney: action.ownerJourney,
              ownerState: action.ownerState,
              locatorWaitMs: 0,
            },
          };
      }
      if (index === triggerActionIndex) {
        armed = armWaitFor(page, state.waitFor || {});
        await locator.scrollIntoViewIfNeeded(options);
        beforeContinuation = await page.evaluate(() => ({
          scrollX: window.scrollX,
          scrollY: window.scrollY,
          path: window.location.pathname,
          origin: window.location.origin,
        }));
      }
      if (action.action === "click") await locator.click(options);
      else if (action.action === "hover") await locator.hover(options);
      else if (action.action === "focus") await locator.focus(options);
      else if (action.action === "fill") await locator.fill(action.value, options);
      else if (action.action === "check") await locator.check(options);
      else if (action.action === "uncheck") await locator.uncheck(options);
      else if (action.action === "press") await locator.press(action.value, options);
      else if (action.action === "selectOption") await locator.selectOption(action.value, options);
      actionTimings.push({
        index,
        action: action.action,
        selector: action.selector,
        durationMs: Date.now() - actionStarted,
        outcome: "completed",
      });
    } catch (error) {
      // Playwright call logs may echo entered values. Keep reports actionable
      // without copying action payloads (which can be credentials or PII).
      let afterFailureObservation = null;
      if (Object.keys(state.afterFailureWaitFor || {}).length) {
        try {
          afterFailureObservation = {
            checked: true,
            waitEvidence: await applyWaitFor(page, state.afterFailureWaitFor, failureArmed),
          };
        } catch (observationError) {
          afterFailureObservation = {
            checked: false,
            error: String(observationError?.message || observationError).slice(0, 512),
          };
        }
      }
      actionTimings.push({
        index,
        action: action.action,
        selector: action.selector,
        durationMs: Date.now() - actionStarted,
        outcome: "failed",
      });
      return {
        beforeContinuation,
        waitEvidence,
        actionTimings,
        handoff: null,
        failure: {
          kind: "interaction-action-failed",
          message: `${action.action} failed for ${action.selector} (${error.name || "interaction error"})`,
          afterFailureObservation,
        },
      };
    }
  }
  try {
    waitEvidence.push(...await applyWaitFor(page, state.waitFor || {}, armed));
  } catch (error) {
    return {
      beforeContinuation,
      waitEvidence,
      actionTimings,
      handoff: null,
      failure: {
        kind: "interaction-readiness-failed",
        message: `interaction readiness failed (${error.name || "wait error"})`,
      },
    };
  }
  return { beforeContinuation, waitEvidence, actionTimings, handoff: null, failure: null };
}

async function verifyContinuation(page, continuation, beforeContinuation) {
  if (!continuation) return { checked: false, findings: [], evidence: null };
  const findings = [];
  const current = await page.evaluate(() => ({
    scrollX: window.scrollX,
    scrollY: window.scrollY,
    path: window.location.pathname,
    origin: window.location.origin,
    viewportWidth: window.innerWidth,
    viewportHeight: window.innerHeight,
  }));
  const anchor = page.locator(continuation.anchor).first();
  let count = 0;
  try {
    count = await anchor.count();
  } catch (error) {
    findings.push({
      severity: "critical",
      rule: "continuation-anchor-invalid",
      message: "Continuation anchor selector could not be evaluated.",
      selector: continuation.anchor,
      textSnippet: "",
      rect: null,
      area: null,
      evidence: { error: error.name || "selector error" },
    });
  }
  let anchorRect = null;
  let anchorVisible = false;
  let anchorRecognizable = false;
  if (count > 0) {
    anchorRect = await anchor.boundingBox().catch(() => null);
    anchorVisible = Boolean(anchorRect) && await anchor.isVisible().catch(() => false);
    anchorRecognizable = await anchor.evaluate((element) => Boolean(
      element.matches("h1,h2,h3,h4,h5,h6,input:not([type='hidden']),select,textarea,[role='heading'],[data-ui-continuation-anchor]")
    )).catch(() => false);
  }
  const inViewport = Boolean(
    anchorVisible &&
    anchorRect.width > 1 &&
    anchorRect.height > 1 &&
    anchorRect.x + anchorRect.width > 0 &&
    anchorRect.y + anchorRect.height > 0 &&
    anchorRect.x < current.viewportWidth &&
    anchorRect.y < current.viewportHeight
  );
  if (!count) {
    findings.push({
      severity: "critical",
      rule: "continuation-anchor-missing",
      message: "Activated journey did not render its declared continuation anchor.",
      selector: continuation.anchor,
      textSnippet: "",
      rect: null,
      area: null,
      evidence: {},
    });
  } else if (!inViewport) {
    findings.push({
      severity: "critical",
      rule: "continuation-anchor-offscreen",
      message: "Activated journey rendered its continuation outside the current viewport.",
      selector: continuation.anchor,
      textSnippet: "",
      rect: anchorRect,
      area: null,
      evidence: {
        viewport: { width: current.viewportWidth, height: current.viewportHeight },
      },
    });
  }
  if (count > 0 && !anchorRecognizable) {
    findings.push({
      severity: "critical",
      rule: "continuation-anchor-not-recognizable",
      message: "Continuation anchor must be the revealed heading, first field, or an explicitly marked recognizable anchor.",
      selector: continuation.anchor,
      textSnippet: "",
      rect: anchorRect,
      area: null,
      evidence: {},
    });
  }
  let focusWithin = null;
  if (continuation.kind === "in-page" && count > 0) {
    const focusRoot = page.locator(continuation.focusWithin).first();
    if (await focusRoot.count().catch(() => 0)) {
      focusWithin = await focusRoot.evaluate((root) => {
        let active = document.activeElement;
        while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
        return Boolean(active && (active === root || root.contains(active)));
      }).catch(() => false);
    } else {
      focusWithin = false;
    }
    if (!focusWithin) {
      findings.push({
        severity: "critical",
        rule: "continuation-focus-missing",
        message: "Activated in-page journey did not move focus into its declared continuation surface.",
        selector: continuation.focusWithin,
        textSnippet: "",
        rect: null,
        area: null,
        evidence: {},
      });
    }
  }
  const scrollDelta = beforeContinuation
    ? Math.max(
        Math.abs(current.scrollX - beforeContinuation.scrollX),
        Math.abs(current.scrollY - beforeContinuation.scrollY),
      )
    : null;
  if (
    continuation.kind === "in-page" &&
    (scrollDelta === null || scrollDelta > continuation.maxScrollDelta)
  ) {
    findings.push({
      severity: "critical",
      rule: "continuation-document-jump",
      message: "Activated in-page journey moved the document instead of continuing in the user's current viewport.",
      selector: continuation.anchor,
      textSnippet: "",
      rect: anchorRect,
      area: null,
      evidence: { scrollDelta, maxScrollDelta: continuation.maxScrollDelta },
    });
  }
  return {
    checked: true,
    findings,
    evidence: {
      kind: continuation.kind,
      anchor: continuation.anchor,
      focusWithin: continuation.focusWithin,
      expectedPath: continuation.expectedPath,
      anchorVisibleInViewport: inViewport,
      anchorRecognizable,
      focusSatisfied: focusWithin,
      scrollDelta,
      maxScrollDelta: continuation.maxScrollDelta,
    },
  };
}

function resolvePlaywright(explicitModuleDir) {
  const candidates = [];
  const cwd = process.cwd();
  if (explicitModuleDir) candidates.push(explicitModuleDir);
  // A canonically installed skill is a direct link into this repository. Resolve
  // the locked project dependency from the script itself, never from the
  // temporary audited working directory.
  candidates.push(path.resolve(path.dirname(VERIFIER_PATH), "../../../ci/playwright/node_modules"));
  candidates.push(cwd);
  if (process.env.NODE_PATH) {
    for (const item of process.env.NODE_PATH.split(path.delimiter)) {
      if (item.trim()) candidates.push(item.trim());
    }
  }
  if (process.platform === "win32") {
    if (process.env.APPDATA) candidates.push(path.join(process.env.APPDATA, "npm", "node_modules"));
    candidates.push(path.join(path.dirname(process.execPath), "node_modules"));
  } else {
    candidates.push(path.resolve(path.dirname(process.execPath), "..", "lib", "node_modules"));
  }
  candidates.push(path.join(os.homedir(), ".cache/codex-runtimes/codex-primary-runtime/dependencies/node/node_modules"));
  const errors = [];
  for (const base of [...new Set(candidates)]) {
    try {
      const direct = path.join(base, "playwright");
      const resolved = fs.existsSync(direct)
        ? require.resolve(direct)
        : require.resolve("playwright", { paths: [base] });
      return require(resolved);
    } catch (error) {
      errors.push(`${base}: ${error.message}`);
    }
  }
  throw new Error(`Cannot resolve Playwright. Checked:\n${errors.join("\n")}`);
}

function localBrowserCandidates(explicitPath) {
  const candidates = [];
  if (explicitPath) candidates.push(explicitPath);
  if (process.platform === "darwin") {
    candidates.push(
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      "/Applications/Chromium.app/Contents/MacOS/Chromium",
      "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
      "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    );
  } else if (process.platform === "win32") {
    const roots = [process.env.PROGRAMFILES, process.env["PROGRAMFILES(X86)"], process.env.LOCALAPPDATA].filter(Boolean);
    for (const root of roots) {
      candidates.push(
        path.join(root, "Google/Chrome/Application/chrome.exe"),
        path.join(root, "Microsoft/Edge/Application/msedge.exe"),
      );
    }
  } else {
    candidates.push("/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser", "/snap/bin/chromium");
  }
  return [...new Set(candidates)].filter((item) => item && fs.existsSync(item));
}

async function launchBrowser(chromium, executablePath) {
  const attempts = [];
  if (executablePath) attempts.push({ label: executablePath, options: { executablePath } });
  attempts.push({ label: "playwright-managed-browser", options: {} });
  for (const candidate of localBrowserCandidates(executablePath)) {
    if (candidate !== executablePath) attempts.push({ label: candidate, options: { executablePath: candidate } });
  }
  const errors = [];
  for (const attempt of attempts) {
    try {
      const browser = await chromium.launch({ headless: true, ...attempt.options });
      return { browser, browserLabel: attempt.label };
    } catch (error) {
      const detail = String(error.message || error)
        .split("\n").slice(0, 8).join("\n");
      errors.push(`${attempt.label}: ${detail}`);
    }
  }
  throw new Error(`Unable to launch a Chromium browser:\n${errors.join("\n")}`);
}

function coordinatorTargets(config) {
  if (!config.fromCoordinator) return [];
  const command = config.coordinatorCommand || "devcoordinator2";
  const args = ["deployment", "list"];
  if (config.coordinatorProject) args.push(config.coordinatorProject);
  const result = spawnSync(command, args, {
    cwd: process.cwd(),
    encoding: "utf8",
    maxBuffer: 10 * 1024 * 1024,
  });
  if (result.status !== 0) {
    throw new Error(`Coordinator deployment discovery failed:\n${result.stderr || result.stdout}`);
  }
  const response = JSON.parse(result.stdout);
  if (!response || response.protocol !== 2 || response.ok !== true || !response.data) {
    throw new Error("Coordinator deployment discovery returned an invalid response");
  }
  const deployments = Array.isArray(response.data.deployments)
    ? response.data.deployments
    : [];
  const targets = [];
  for (const item of deployments) {
    if (!item || !Number.isInteger(item.route_port)) continue;
    if (config.onlyCurrent && item.status && item.status !== "running") continue;
    if (config.onlyCurrent && item.state && item.state !== "running") continue;
    const url = `http://127.0.0.1:${item.route_port}`;
    targets.push({
      ...(config.targetDefaults || {}),
      url,
      name: item.name || url,
      project: item.repository_name || null,
      source: "coordinator",
      healthUrl: null,
      status: item.state || null,
    });
  }
  return targets;
}

function sanitizeFilePart(value) {
  return String(value).replace(/[^a-z0-9_.-]+/gi, "_").replace(/^_+|_+$/g, "").slice(0, 80) || "page";
}

// Runs a full-page scroll pass in viewport-height steps so lazy-loaded content and
// IntersectionObservers and layout paint across two animation frames between
// steps. Robust to pages that grow while scrolling via a hard iteration cap.
async function scrollThroughPage(page, { maxIterations = 30 } = {}) {
  const metrics = { scrollPasses: 0, scrolledTo: 0, maxScrollHeight: 0, capped: false };
  const originalY = await page.evaluate(() => window.scrollY).catch(() => 0);
  for (let i = 0; i < maxIterations; i += 1) {
    const state = await page
      .evaluate(() => {
        const scrolling = document.scrollingElement || document.documentElement;
        const step = window.innerHeight || 600;
        const before = window.scrollY;
        const maxScroll = Math.max(0, scrolling.scrollHeight - window.innerHeight);
        const nextY = Math.min(before + step, maxScroll);
        window.scrollTo(0, nextY);
        return {
          scrollHeight: scrolling.scrollHeight,
          innerHeight: window.innerHeight,
          scrollY: window.scrollY,
          atBottom: window.scrollY >= maxScroll - 1,
        };
      })
      .catch(() => null);
    if (!state) break;
    metrics.scrollPasses += 1;
    metrics.scrolledTo = Math.max(metrics.scrolledTo, Math.round(state.scrollY));
    metrics.maxScrollHeight = Math.max(metrics.maxScrollHeight, Math.round(state.scrollHeight));
    await waitForRenderFrames(page, 2);
    if (state.atBottom) break;
    if (i === maxIterations - 1) metrics.capped = true;
  }
  await page.evaluate((y) => window.scrollTo(0, y), originalY).catch(() => {});
  await waitForRenderFrames(page, 2).catch(() => {});
  return metrics;
}

function journeyHierarchyVerifier(contract) {
  const findings = [];
  const rows = [];
  const deepQueryAll = (selector) => {
    const matches = [];
    const roots = [document];
    while (roots.length) {
      const root = roots.shift();
      matches.push(...root.querySelectorAll(selector));
      for (const element of root.querySelectorAll("*")) {
        if (element.shadowRoot) roots.push(element.shadowRoot);
      }
    }
    return matches;
  };
  const visible = (element) => {
    const style = getComputedStyle(element);
    if (style.display === "none" || style.visibility === "hidden" || Number(style.opacity) <= 0.01) return false;
    if (typeof element.checkVisibility === "function" && !element.checkVisibility()) return false;
    const rect = element.getBoundingClientRect();
    return rect.width > 1 && rect.height > 1;
  };
  const rectObject = (rect) => ({
    x: Math.round(rect.x * 100) / 100,
    y: Math.round(rect.y * 100) / 100,
    width: Math.round(rect.width * 100) / 100,
    height: Math.round(rect.height * 100) / 100,
    top: Math.round(rect.top * 100) / 100,
    right: Math.round(rect.right * 100) / 100,
    bottom: Math.round(rect.bottom * 100) / 100,
    left: Math.round(rect.left * 100) / 100,
  });
  for (const region of contract.regions || []) {
    let elements = [];
    try {
      elements = deepQueryAll(region.selector);
    } catch (error) {
      findings.push({
        severity: "critical",
        rule: "invalid-journey-region-selector",
        message: "A declared journey-region selector could not be evaluated.",
        selector: region.selector,
        textSnippet: "",
        rect: null,
        area: null,
        evidence: { region: region.name, role: region.role, error: error.name || "selector error" },
      });
      continue;
    }
    for (const element of elements) {
      const rect = element.getBoundingClientRect();
      const isVisible = visible(element);
      const visibleTop = Math.max(0, rect.top);
      const visibleBottom = Math.min(window.innerHeight, rect.bottom);
      const visibleHeight = isVisible ? Math.max(0, visibleBottom - visibleTop) : 0;
      rows.push({
        name: region.name,
        selector: region.selector,
        role: region.role,
        journey: region.journey,
        reason: region.reason,
        visible: isVisible,
        inViewport: visibleHeight > 1 && rect.right > 0 && rect.left < window.innerWidth,
        visibleHeight,
        viewportHeightFraction: visibleHeight / Math.max(1, window.innerHeight),
        rect: rectObject(rect),
      });
    }
  }
  const primaryRows = rows.filter(
    (row) => row.role === "primary-content" && row.journey === contract.primaryJourney && row.visible,
  );
  const primaryInViewport = primaryRows.filter((row) => row.inViewport);
  if (!primaryRows.length) {
    findings.push({
      severity: "critical",
      rule: "primary-journey-content-missing",
      message: "The declared primary journey has no rendered visible primary-content region.",
      selector: "document",
      textSnippet: "",
      rect: null,
      area: null,
      evidence: { primaryJourney: contract.primaryJourney },
    });
  } else if (!primaryInViewport.some((row) => row.visibleHeight >= 24)) {
    findings.push({
      severity: "critical",
      rule: "primary-journey-outside-initial-viewport",
      message: "The declared primary journey is not recognizably visible in the initial viewport.",
      selector: primaryRows[0].selector,
      textSnippet: "",
      rect: primaryRows[0].rect,
      area: null,
      evidence: { primaryJourney: contract.primaryJourney, minimumVisibleHeight: 24 },
    });
  } else {
    const strongest = primaryInViewport.reduce(
      (best, row) => row.viewportHeightFraction > best.viewportHeightFraction ? row : best,
      primaryInViewport[0],
    );
    if (strongest.viewportHeightFraction < 0.20) {
      findings.push({
        severity: "warning",
        rule: "primary-journey-low-initial-visibility",
        message: "The primary journey occupies less than 20% of the initial viewport height; review whether it is recognizable enough.",
        selector: strongest.selector,
        textSnippet: "",
        rect: strongest.rect,
        area: null,
        evidence: {
          primaryJourney: contract.primaryJourney,
          viewportHeightFraction: Math.round(strongest.viewportHeightFraction * 1000) / 1000,
        },
      });
    }
  }
  if (primaryRows.length) {
    const primaryTop = Math.min(...primaryRows.map((row) => row.rect.top));
    for (const row of rows) {
      if (!row.visible || row.rect.top >= primaryTop - 8) continue;
      if (row.role === "workflow-surface" && row.journey !== contract.primaryJourney) {
        findings.push({
          severity: "critical",
          rule: "secondary-workflow-precedes-primary",
          message: "A lower-priority journey surface appears before the destination's primary journey content.",
          selector: row.selector,
          textSnippet: "",
          rect: row.rect,
          area: null,
          evidence: {
            primaryJourney: contract.primaryJourney,
            secondaryJourney: row.journey,
            secondaryTop: row.rect.top,
            primaryTop,
          },
        });
      }
      if (row.role === "supporting" && row.viewportHeightFraction > 0.25) {
        findings.push({
          severity: "critical",
          rule: "supporting-content-dominates-primary",
          message: "Supporting content consumes more than a compact share of the viewport before the primary journey.",
          selector: row.selector,
          textSnippet: "",
          rect: row.rect,
          area: null,
          evidence: {
            viewportHeightFraction: Math.round(row.viewportHeightFraction * 1000) / 1000,
            primaryTop,
          },
        });
      }
    }
  }
  return {
    primaryJourney: contract.primaryJourney,
    viewport: { width: window.innerWidth, height: window.innerHeight, scrollX: window.scrollX, scrollY: window.scrollY },
    regions: rows,
    findings,
  };
}

function pageVerifier() {
  const config = window.__FORMAL_WEB_UI_CONFIG__;
  const controlSelector = [
    "button",
    "a[href]",
    "input",
    "select",
    "textarea",
    "summary",
    "[role='button']",
    "[role='link']",
    "[role='checkbox']",
    "[role='tab']",
    "[role='menuitem']",
    "[tabindex]:not([tabindex='-1'])",
    "[contenteditable='true']",
  ].join(",");
  const textSelector = [
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "p",
    "li",
    "td",
    "th",
    "label",
    "span",
    "[data-ui-verify-text]",
  ].join(",");
  const selectorLists = {
    ignore: config.ignore || [],
    allowTruncation: config.allowTruncation || [],
    allowOverlap: config.allowOverlap || [],
    allowContrast: config.allowContrast || [],
    themeExceptions: config.themeExceptions || [],
  };
  const findings = [];
  const unmeasurableContrast = [];
  const ellipsisTruncations = [];
  const controlTextMeasurements = [];
  const controlContainmentMeasurements = [];
  const contentInsetMeasurements = [];
  const hiddenTextLike = { displayNone: 0, visibilityHidden: 0, zeroOpacity: 0, zeroSize: 0 };
  let pendingMedia = 0;
  const allElements = [];
  const roots = [document];
  let inspectedOpenShadowRoots = 0;
  while (roots.length) {
    const root = roots.shift();
    for (const element of root.querySelectorAll("*")) {
      allElements.push(element);
      if (element.shadowRoot) {
        inspectedOpenShadowRoots += 1;
        roots.push(element.shadowRoot);
      }
    }
  }
  const deepQueryAll = (selector) => allElements.filter((element) => {
    try {
      return element.matches(selector);
    } catch {
      return false;
    }
  });
  const composedParent = (element) => {
    if (!element || element.nodeType !== Node.ELEMENT_NODE) return null;
    return element.parentElement || element.getRootNode?.()?.host || null;
  };

  const nowRect = (el) => el.getBoundingClientRect();
  const round = (value) => Math.round(value * 100) / 100;
  const rectObj = (rect) => ({
    x: round(rect.x),
    y: round(rect.y),
    width: round(rect.width),
    height: round(rect.height),
    right: round(rect.right),
    bottom: round(rect.bottom),
  });
  const textOf = (el) => (el.innerText || el.textContent || "").replace(/\s+/g, " ").trim();
  const snippet = (el) => {
    // Native form-control text may be typed, selected, or otherwise sensitive.
    // Findings identify the control without serializing its value, placeholder,
    // textarea content, or option labels.
    const rendered = el.matches?.("input,textarea,select") ? "" : textOf(el);
    return (rendered || el.getAttribute("aria-label") || el.getAttribute("title") || el.tagName).slice(0, 140);
  };
  const selectorPath = (el) => {
    if (el.id) return `#${CSS.escape(el.id)}`;
    const parts = [];
    let node = el;
    while (node && node.nodeType === Node.ELEMENT_NODE && node !== document.body && parts.length < 5) {
      let part = node.localName;
      if (node.classList && node.classList.length) {
        part += `.${Array.from(node.classList).slice(0, 2).map((item) => CSS.escape(item)).join(".")}`;
      }
      const parent = node.parentElement;
      if (parent) {
        const siblings = Array.from(parent.children).filter((child) => child.localName === node.localName);
        if (siblings.length > 1) part += `:nth-of-type(${siblings.indexOf(node) + 1})`;
      }
      parts.unshift(part);
      if (parent) {
        node = parent;
      } else {
        const host = node.getRootNode?.()?.host;
        if (host) {
          parts.unshift(">>>");
          node = host;
        } else {
          node = null;
        }
      }
    }
    return parts.join(" > ") || el.localName || "element";
  };
  const matchesList = (el, entries) => {
    for (const entry of entries) {
      try {
        if (el.matches(entry.selector) || el.closest(entry.selector)) return entry.reason || "configured";
      } catch {
        continue;
      }
    }
    return "";
  };
  const hasAttrReason = (el, attr) => {
    const owner = el.closest(`[${attr}]`);
    return owner ? owner.getAttribute(attr) || attr : "";
  };
  // Framework dev-tooling overlays (Next.js dev badge/error portal, build
  // watcher) are injected by the dev server, absent from production builds,
  // and sit above real content by design — they are not part of the page
  // under test and must not count as occluders or candidates.
  const DEV_OVERLAY_SELECTOR = "nextjs-portal, #__next-build-watcher, [data-nextjs-toast]";
  const isDevOverlay = (el) => Boolean(el.closest && el.closest(DEV_OVERLAY_SELECTOR));
  const isIgnored = (el) => Boolean(isDevOverlay(el) || hasAttrReason(el, "data-ui-verify-ignore") || matchesList(el, selectorLists.ignore));
  const truncationReason = (el) => hasAttrReason(el, "data-ui-allow-truncation") || matchesList(el, selectorLists.allowTruncation);
  const overlapReason = (el) => hasAttrReason(el, "data-ui-allow-overlap") || matchesList(el, selectorLists.allowOverlap);
  const contrastReason = (el) => hasAttrReason(el, "data-ui-allow-contrast") || matchesList(el, selectorLists.allowContrast);
  const themeExceptionReason = (el) => hasAttrReason(el, "data-ui-theme-exception") || matchesList(el, selectorLists.themeExceptions);

  const styleCache = new WeakMap();
  const cs = (el) => {
    let style = styleCache.get(el);
    if (!style) {
      style = getComputedStyle(el);
      styleCache.set(el, style);
    }
    return style;
  };
  const opacityCache = new WeakMap();
  const effectiveOpacity = (el) => {
    if (!el || el.nodeType !== Node.ELEMENT_NODE) return 1;
    const cached = opacityCache.get(el);
    if (cached !== undefined) return cached;
    const value = Number(cs(el).opacity || 1) * effectiveOpacity(composedParent(el));
    opacityCache.set(el, value);
    return value;
  };

  // Complex-artifact detection is token-bounded so ordinary sections named
  // "roadmap", "sitemap", or "org-chart-team" are NOT excluded from checks.
  // An element is artifact context only when an ancestor is a real svg/canvas,
  // matches a known visualization/map library token, or carries a generic
  // map/chart token AND actually contains a substantial svg/canvas/video.
  const ARTIFACT_LIB_TOKENS = /(^|[^a-z0-9])(leaflet|mapbox|maplibre|gm-style|recharts|echarts|highcharts|chartjs|apexcharts|plotly|nivo|visx|vega|cesium|deckgl|deck-gl|openlayers|ol-viewport)([^a-z0-9]|$)/;
  const ARTIFACT_GENERIC_TOKENS = /(^|[^a-z0-9])(map|chart|graph|plot|gauge|sparkline|axis|legend|marker|cluster|heatmap|treemap|diagram)([^a-z0-9]|$)/;
  const CAROUSEL_TOKENS = /(^|[^a-z0-9])(carousel|swiper|slider|slick|embla|glide|flickity|splide|marquee|ticker)([^a-z0-9]|$)/;
  const markerText = (node) => `${node.localName || ""} ${node.id || ""} ${typeof node.className === "string" ? node.className : node.className?.baseVal || ""}`.toLowerCase();
  const artifactCache = new WeakMap();
  const nodeIsArtifact = (node) => {
    const cached = artifactCache.get(node);
    if (cached !== undefined) return cached;
    let isArtifact = false;
    if (node.localName === "svg" || node.localName === "canvas" || node.ownerSVGElement) {
      isArtifact = true;
    } else {
      const marker = markerText(node);
      if (ARTIFACT_LIB_TOKENS.test(marker)) {
        isArtifact = true;
      } else if (ARTIFACT_GENERIC_TOKENS.test(marker)) {
        for (const media of node.querySelectorAll("svg,canvas,video")) {
          const rect = media.getBoundingClientRect();
          if (rect.width * rect.height >= 10000) {
            isArtifact = true;
            break;
          }
        }
      }
    }
    artifactCache.set(node, isArtifact);
    return isArtifact;
  };
  const complexArtifactContext = (el) => {
    let node = el;
    while (node && node.nodeType === Node.ELEMENT_NODE && node !== document.body) {
      if (nodeIsArtifact(node)) return true;
      node = composedParent(node);
    }
    return false;
  };

  const modalContains = (ancestor, element) => {
    let node = element;
    while (node) {
      if (node === ancestor) return true;
      node = composedParent(node);
    }
    return false;
  };
  let modalFocus = document.activeElement;
  while (modalFocus?.shadowRoot?.activeElement) modalFocus = modalFocus.shadowRoot.activeElement;
  const nativeModals = allElements.filter((element) => element.matches('dialog:modal'));
  const activeModal = nativeModals.findLast((modal) => modalContains(modal, modalFocus)) || nativeModals.at(-1);
  const rendered = (el) => {
    const style = cs(el);
    if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") return false;
    if (effectiveOpacity(el) <= 0.01) return false;
    // Closed <details> (and content-visibility: hidden subtrees) keep layout
    // boxes for content the browser does not render; checkVisibility() is the
    // only reliable signal that such content is not actually shown.
    if (typeof el.checkVisibility === "function" && !el.checkVisibility()) return false;
    const rect = nowRect(el);
    return rect.width > 1 && rect.height > 1;
  };
  const customModal = activeModal ? null : allElements.findLast((element) =>
    element.matches('[role="dialog"][aria-modal="true"], [role="alertdialog"][aria-modal="true"]') &&
    rendered(element) && modalContains(element, modalFocus)
  );
  const visible = (el) => {
    if (activeModal && !modalContains(activeModal, el) && !modalContains(el, activeModal)) return false;
    if (customModal && !modalContains(customModal, el) && !modalContains(el, customModal)) {
      let inert = false;
      let hidden = false;
      for (let ancestor = el; ancestor; ancestor = composedParent(ancestor)) {
        inert ||= ancestor.hasAttribute("inert");
        hidden ||= ancestor.getAttribute("aria-hidden") === "true";
      }
      if (inert && hidden) return false;
    }
    return rendered(el);
  };
  const hasVisibleElementChild = (el) => Array.from(el.children).some((child) => visible(child));
  const hasDirectText = (el) => {
    for (const node of el.childNodes) {
      if (node.nodeType === Node.TEXT_NODE && node.textContent && node.textContent.trim().length > 0) return true;
    }
    return false;
  };
  const isControl = (el) => el.matches(controlSelector);
  const isTextCandidate = (el) => el.matches(textSelector) && textOf(el).length > 0;
  const classicLeafText = (el) => isTextCandidate(el) && (!hasVisibleElementChild(el) || el.matches("h1,h2,h3,h4,h5,h6,p,li,td,th,label"));
  // Any element that directly owns rendered text (including div/section/dd/etc.)
  // is a text candidate; the fixed tag list alone misses most modern app text.
  const isLeafText = (el) => hasDirectText(el) || classicLeafText(el);

  const MAX_PER_RULE = 40;
  const ruleCounts = {};
  const suppressed = {};
  const add = (severity, rule, el, message, extra = {}) => {
    ruleCounts[rule] = (ruleCounts[rule] || 0) + 1;
    if (ruleCounts[rule] > MAX_PER_RULE) {
      suppressed[rule] = (suppressed[rule] || 0) + 1;
      return;
    }
    findings.push({
      severity,
      rule,
      message,
      selector: Object.hasOwn(extra, "selector")
        ? String(extra.selector || "document")
        : (el ? selectorPath(el) : "document"),
      textSnippet: extra.redactText
        ? ""
        : (Object.hasOwn(extra, "textSnippet") ? String(extra.textSnippet || "") : (el ? snippet(el) : "")),
      rect: Object.hasOwn(extra, "rect")
        ? extra.rect
        : (el ? rectObj(nowRect(el)) : null),
      area: extra.area || null,
      evidence: extra.evidence || {},
    });
  };
  const doc = document.documentElement;
  if (doc.scrollWidth > doc.clientWidth + 2) {
    findings.push({
      severity: "critical",
      rule: "document-horizontal-overflow",
      message: "Document scrollWidth exceeds viewport width.",
      selector: "document.documentElement",
      textSnippet: "",
      rect: null,
      area: null,
      evidence: { scrollWidth: doc.scrollWidth, clientWidth: doc.clientWidth },
    });
  }

  const configuredAreaRoots = [];
  for (const area of config.areas || []) {
    try {
      for (const el of deepQueryAll(area.selector)) configuredAreaRoots.push({ name: area.name, el });
    } catch {
      findings.push({
        severity: "warning",
        rule: "invalid-area-selector",
        message: `Area selector could not be evaluated: ${area.selector}`,
        selector: area.selector,
        textSnippet: "",
        rect: null,
        area: area.name,
        evidence: {},
      });
    }
  }
  for (const el of deepQueryAll("[data-ui-verify-area]")) {
    configuredAreaRoots.push({ name: el.getAttribute("data-ui-verify-area") || selectorPath(el), el });
  }

  const SKIP_TAGS = new Set([
    "script", "style", "template", "noscript", "meta", "link", "title", "base",
    "br", "hr", "wbr", "source", "track", "param", "slot", "option", "optgroup",
    "datalist", "iframe", "object", "embed", "area", "map", "svg", "canvas", "portal",
  ]);
  const candidates = [];
  for (const el of allElements) {
    if (SKIP_TAGS.has(el.localName) || el.ownerSVGElement) continue;
    if (isIgnored(el)) continue;
    const textLike = isControl(el) || isLeafText(el);
    if (!textLike && !el.matches("img,video")) continue;
    if (!visible(el)) {
      // Inventory text/controls that exist but are invisible so hidden-content
      // regressions are at least countable in evidence.
      if (textLike) {
        const style = cs(el);
        if (style.display === "none") hiddenTextLike.displayNone += 1;
        else if (style.visibility === "hidden" || style.visibility === "collapse") hiddenTextLike.visibilityHidden += 1;
        else if (effectiveOpacity(el) <= 0.01) hiddenTextLike.zeroOpacity += 1;
        else hiddenTextLike.zeroSize += 1;
      }
      continue;
    }
    candidates.push(el);
  }

  const svgRectsIntersect = (first, second) => {
    const width = Math.min(first.right, second.right) - Math.max(first.left, second.left);
    const height = Math.min(first.bottom, second.bottom) - Math.max(first.top, second.top);
    return width > 1 && height > 1 ? { width: round(width), height: round(height) } : null;
  };
  const svgPaintVisible = (paint, opacity) => {
    if (!paint || paint === "none" || Number(opacity || 1) <= 0.01) return false;
    const color = parseCssColor(paint);
    return !color || color.a > 0.01;
  };
  const renderedSvgText = (el) => {
    const style = cs(el);
    return effectiveOpacity(el) > 0.01 && (
      svgPaintVisible(style.fill, style.fillOpacity)
      || svgPaintVisible(style.stroke, style.strokeOpacity)
    );
  };
  const paintedSvgShape = (el) => {
    if (!el.matches("rect,path,circle,ellipse,polygon")) return false;
    const style = cs(el);
    return effectiveOpacity(el) > 0.01 && svgPaintVisible(style.fill, style.fillOpacity);
  };
  for (const protectedText of deepQueryAll("text[data-ui-verify-svg-overlap]")) {
    if (!protectedText.ownerSVGElement || isIgnored(protectedText)
        || overlapReason(protectedText) || !visible(protectedText)
        || !renderedSvgText(protectedText)) continue;
    const protectedRect = nowRect(protectedText);
    if (protectedRect.width <= 1 || protectedRect.height <= 1) continue;
    const collisions = [];
    for (const peer of protectedText.ownerSVGElement.querySelectorAll(
      "text,rect,path,circle,ellipse,polygon",
    )) {
      if (peer.ownerSVGElement !== protectedText.ownerSVGElement
          || peer === protectedText || protectedText.contains(peer) || peer.contains(protectedText)
          || isIgnored(peer) || overlapReason(peer) || !visible(peer)) continue;
      const kind = peer.localName === "text"
        ? (renderedSvgText(peer) ? "text" : "")
        : (paintedSvgShape(peer) ? "filled-shape" : "");
      if (!kind) continue;
      const peerRect = nowRect(peer);
      if (peerRect.width <= 1 || peerRect.height <= 1) continue;
      const intersection = svgRectsIntersect(protectedRect, peerRect);
      if (!intersection) continue;
      collisions.push({
        selector: selectorPath(peer),
        kind,
        rect: rectObj(peerRect),
        intersection,
      });
      if (collisions.length >= 8) break;
    }
    if (collisions.length) {
      add("critical", "svg-internal-overlap", protectedText,
        "Protected SVG text intersects another rendered SVG label or filled shape.", {
          redactText: true,
          evidence: { protectedRect: rectObj(protectedRect), collisions },
        });
    }
  }

  const px = (value) => {
    const parsed = Number.parseFloat(value);
    return Number.isFinite(parsed) ? parsed : 0;
  };
  const applyTextMeasurementStyle = (mirror, style) => {
    for (const property of [
      "font", "fontKerning", "fontFeatureSettings", "fontVariationSettings",
      "letterSpacing", "wordSpacing", "textTransform", "textRendering",
    ]) {
      try {
        mirror.style[property] = style[property];
      } catch {
        // Older engines may not expose every text-rendering property.
      }
    }
    mirror.style.setProperty("position", "fixed", "important");
    mirror.style.setProperty("left", "-100000px", "important");
    mirror.style.setProperty("top", "0", "important");
    mirror.style.setProperty("visibility", "hidden", "important");
    mirror.style.setProperty("white-space", "pre", "important");
    mirror.style.setProperty("width", "max-content", "important");
    mirror.style.setProperty("max-width", "none", "important");
    mirror.style.setProperty("min-width", "0", "important");
    mirror.style.setProperty("padding", "0", "important");
    mirror.style.setProperty("border", "0", "important");
  };
  const widestRenderedText = (values, style) => {
    const mirror = document.createElement("span");
    applyTextMeasurementStyle(mirror, style);
    (document.body || document.documentElement).append(mirror);
    let value = "";
    let width = 0;
    for (const candidate of values) {
      mirror.textContent = candidate;
      const candidateWidth = mirror.getBoundingClientRect().width;
      if (candidateWidth > width) {
        value = candidate;
        width = candidateWidth;
      }
    }
    mirror.remove();
    return { value, width };
  };
  const renderedTextWidth = (value, style) => widestRenderedText([value], style).width;
  const selectNativeReserve = (el, label, labelWidth, style) => {
    const clone = document.createElement("select");
    if (el.multiple) clone.multiple = true;
    if (el.size > 0) clone.size = el.size;
    clone.dir = el.dir;
    const option = document.createElement("option");
    option.textContent = label;
    option.selected = true;
    clone.append(option);
    applyTextMeasurementStyle(clone, style);
    clone.style.setProperty("box-sizing", style.boxSizing, "important");
    clone.style.setProperty("appearance", style.appearance, "important");
    clone.style.setProperty("padding-left", style.paddingLeft, "important");
    clone.style.setProperty("padding-right", style.paddingRight, "important");
    clone.style.setProperty("padding-top", style.paddingTop, "important");
    clone.style.setProperty("padding-bottom", style.paddingBottom, "important");
    clone.style.setProperty("border-left", style.borderLeft, "important");
    clone.style.setProperty("border-right", style.borderRight, "important");
    clone.style.setProperty("border-top", style.borderTop, "important");
    clone.style.setProperty("border-bottom", style.borderBottom, "important");
    clone.style.setProperty("width", "auto", "important");
    (document.body || document.documentElement).append(clone);
    const borderWidth = px(style.borderLeftWidth) + px(style.borderRightWidth);
    const paddingWidth = px(style.paddingLeft) + px(style.paddingRight);
    const reserve = Math.max(0, clone.getBoundingClientRect().width - borderWidth - paddingWidth - labelWidth);
    clone.remove();
    return reserve;
  };
  const nativeControlText = (el) => {
    if (el.matches("input,textarea")) {
      if (String(el.value || "").length > 0) return null;
      if (el.matches("textarea") && String(el.wrap || "soft").toLowerCase() !== "off") return null;
      const placeholder = el.getAttribute("placeholder") || "";
      if (!placeholder) return null;
      const style = getComputedStyle(el, "::placeholder");
      return {
        kind: "placeholder",
        labelCount: 1,
        textWidth: renderedTextWidth(placeholder, style),
        style,
        nativeReserve: 0,
      };
    }
    if (el.matches("select")) {
      const labels = Array.from(el.options || []).map((option) => option.label || option.text || "");
      if (!labels.length) return null;
      const style = cs(el);
      const widest = widestRenderedText(labels, style);
      return {
        kind: "option-set",
        labelCount: labels.length,
        textWidth: widest.width,
        style,
        nativeReserve: selectNativeReserve(el, widest.value, widest.width, style),
      };
    }
    return null;
  };
  for (const el of candidates.filter((candidate) => candidate.matches("input,textarea,select"))) {
    const measurement = nativeControlText(el);
    if (!measurement) continue;
    const style = cs(el);
    const paddingWidth = px(style.paddingLeft) + px(style.paddingRight);
    const textIndent = Math.max(0, px(measurement.style.textIndent || style.textIndent));
    const availableInnerWidth = Math.max(
      0,
      el.clientWidth - paddingWidth - measurement.nativeReserve - textIndent,
    );
    const clippedBy = Math.max(0, measurement.textWidth - availableInnerWidth);
    const evidence = {
      controlTextKind: measurement.kind,
      labelCount: measurement.labelCount,
      measuredTextWidth: round(measurement.textWidth),
      availableInnerWidth: round(availableInnerWidth),
      nativeAffordanceWidth: round(measurement.nativeReserve),
      clippedBy: round(clippedBy),
    };
    if (controlTextMeasurements.length < 200) {
      controlTextMeasurements.push({
        selector: selectorPath(el),
        ...evidence,
        clipped: clippedBy > 1,
      });
    }
    if (clippedBy <= 1) continue;
    const allowance = truncationReason(el);
    if (allowance) {
      add("warning", "allowed-truncation", el, "Native control text exceeds its inner content width but has an explicit truncation allowance.", {
        redactText: true,
        evidence: { ...evidence, reason: allowance },
      });
    } else {
      add("critical", "control-text-clipped", el, "Native control text exceeds the control's real inner content width.", {
        redactText: true,
        evidence,
      });
    }
  }
  const activeHorizontalScroller = (element) => {
    const style = cs(element);
    return ["auto", "scroll", "overlay"].includes(style.overflowX) &&
      element.scrollWidth > element.clientWidth + 1;
  };
  const nonContainerDisplays = new Set([
    "contents", "inline", "table-row", "table-row-group", "table-header-group", "table-footer-group",
  ]);
  for (const el of candidates.filter((candidate) => candidate.matches("input,textarea,select"))) {
    const controlStyle = cs(el);
    if (["absolute", "fixed"].includes(controlStyle.position)) continue;
    const ancestors = [];
    for (let ancestor = composedParent(el);
      ancestor && ancestor !== document.body && ancestor !== document.documentElement;
      ancestor = composedParent(ancestor)) {
      ancestors.push(ancestor);
    }
    if (ancestors.some(activeHorizontalScroller)) continue;
    const controlRect = nowRect(el);
    for (const ancestor of ancestors) {
      const ancestorStyle = cs(ancestor);
      if (nonContainerDisplays.has(ancestorStyle.display)) continue;
      const ancestorRect = nowRect(ancestor);
      if (ancestorRect.width <= 0 || ancestorRect.height <= 0) continue;
      const innerLeft = ancestorRect.left + px(ancestorStyle.borderLeftWidth);
      const innerRight = ancestorRect.right - px(ancestorStyle.borderRightWidth);
      const cutLeft = Math.max(0, innerLeft - controlRect.left);
      const cutRight = Math.max(0, controlRect.right - innerRight);
      if (Math.max(cutLeft, cutRight) <= 2) continue;
      const evidence = {
        ownerSelector: selectorPath(ancestor),
        controlRect: rectObj(controlRect),
        ownerRect: rectObj(ancestorRect),
        ownerInnerWidth: round(Math.max(0, innerRight - innerLeft)),
        cutLeft: round(cutLeft),
        cutRight: round(cutRight),
        overflowFraction: round((cutLeft + cutRight) / Math.max(1, controlRect.width)),
      };
      const allowance = overlapReason(el);
      if (controlContainmentMeasurements.length < 200) {
        controlContainmentMeasurements.push({
          selector: selectorPath(el),
          ...evidence,
          allowed: Boolean(allowance),
        });
      }
      if (allowance) {
        add("warning", "allowed-overlap", el, "A native form control escapes a non-scrollable layout container under an explicit overlap allowance.", {
          redactText: true,
          evidence: { ...evidence, reason: allowance },
        });
      } else {
        add("critical", "control-outside-container", el, "A native form control escapes a non-scrollable layout container.", {
          redactText: true,
          evidence,
        });
      }
      break;
    }
  }

  const contentInsetContracts = [];
  for (const configured of config.contentInsets || []) {
    try {
      for (const el of deepQueryAll(configured.selector)) {
        contentInsetContracts.push({ ...configured, el, source: "config" });
      }
    } catch {
      findings.push({
        severity: "warning",
        rule: "invalid-content-inset-selector",
        message: "A configured content-inset selector could not be evaluated.",
        selector: configured.selector,
        textSnippet: "",
        rect: null,
        area: null,
        evidence: { name: configured.name },
      });
    }
  }
  for (const el of deepQueryAll("[data-ui-verify-min-content-inset]")) {
    const min = Number(el.getAttribute("data-ui-verify-min-content-inset"));
    if (!Number.isFinite(min) || min <= 0) {
      add("warning", "invalid-content-inset-contract", el, "The content-inset attribute must contain a positive CSS-pixel value.", {
        redactText: true,
      });
      continue;
    }
    contentInsetContracts.push({
      el,
      min,
      name: el.getAttribute("data-ui-verify-area") || selectorPath(el),
      source: "markup",
    });
  }
  const composedWithin = (root, element) => {
    let node = element;
    while (node && node.nodeType === Node.ELEMENT_NODE) {
      if (node === root) return true;
      node = composedParent(node);
    }
    return false;
  };
  const directTextRects = (element) => {
    const rects = [];
    for (const node of element.childNodes) {
      if (node.nodeType !== Node.TEXT_NODE || !node.textContent?.trim()) continue;
      try {
        const range = document.createRange();
        range.selectNodeContents(node);
        for (const rect of range.getClientRects()) {
          if (rect.width > 0.5 && rect.height > 0.5) rects.push(rect);
        }
        range.detach?.();
      } catch {
        // A transiently detached text node is not measurable evidence.
      }
    }
    return rects;
  };
  const seenInsetContracts = new Set();
  for (const contract of contentInsetContracts) {
    const root = contract.el;
    if (!visible(root) || isIgnored(root)) continue;
    const contractKey = `${selectorPath(root)}\u0000${contract.min}`;
    if (seenInsetContracts.has(contractKey)) continue;
    seenInsetContracts.add(contractKey);
    const rootRect = nowRect(root);
    const inner = {
      left: rootRect.left + (root.clientLeft || 0),
      top: rootRect.top + (root.clientTop || 0),
    };
    inner.right = inner.left + root.clientWidth;
    inner.bottom = inner.top + root.clientHeight;
    const fragments = [];
    for (const element of allElements) {
      if (!composedWithin(root, element) || !visible(element) || isIgnored(element)) continue;
      fragments.push(...directTextRects(element));
      if (element !== root && isControl(element)) fragments.push(nowRect(element));
    }
    const observed = { left: Infinity, right: Infinity, top: Infinity, bottom: Infinity };
    for (const fragment of fragments) {
      observed.left = Math.min(observed.left, fragment.left - inner.left);
      observed.right = Math.min(observed.right, inner.right - fragment.right);
      observed.top = Math.min(observed.top, fragment.top - inner.top);
      observed.bottom = Math.min(observed.bottom, inner.bottom - fragment.bottom);
    }
    const roundedObserved = Object.fromEntries(
      Object.entries(observed).map(([side, value]) => [side, Number.isFinite(value) ? round(value) : null]),
    );
    const failingSides = Object.entries(observed)
      .filter(([, value]) => Number.isFinite(value) && value < contract.min - 0.5)
      .map(([side]) => side);
    const entry = {
      selector: selectorPath(root),
      name: contract.name,
      source: contract.source,
      requiredInset: round(contract.min),
      observedInset: roundedObserved,
      measuredFragments: fragments.length,
      status: fragments.length ? (failingSides.length ? "failed" : "passed") : "no-rendered-content",
      failingSides,
    };
    contentInsetMeasurements.push(entry);
    if (failingSides.length) {
      add("critical", "content-inset-below-minimum", root, "Declared important content is closer to the container edge than its minimum readable inset.", {
        redactText: true,
        evidence: entry,
      });
    }
  }

  const iframeCount = deepQueryAll("iframe").length;
  const notInspected = {
    openShadowRoots: 0,
    iframes: config.inspectFramesExternally ? 0 : iframeCount,
    inspectedOpenShadowRoots,
    discoveredOpenShadowRoots: inspectedOpenShadowRoots,
    discoveredIframes: iframeCount,
  };
  if (notInspected.iframes > 0) {
    findings.push({
      severity: "warning",
      rule: "not-inspected",
      message: `${notInspected.iframes} iframe(s) were not inspected; findings may be incomplete.`,
      selector: "document",
      textSnippet: "",
      rect: null,
      area: null,
      evidence: notInspected,
    });
  }

  const establishesContainingBlock = (style) =>
    style.position !== "static" ||
    style.transform !== "none" ||
    style.perspective !== "none" ||
    (style.filter && style.filter !== "none") ||
    (style.backdropFilter && style.backdropFilter !== "none") ||
    (style.contain || "").includes("paint") ||
    (style.contain || "").includes("layout") ||
    (style.willChange || "").includes("transform");
  const fixedContextCache = new WeakMap();
  const hasFixedContext = (el) => {
    if (!el || el.nodeType !== Node.ELEMENT_NODE) return false;
    const cached = fixedContextCache.get(el);
    if (cached !== undefined) return cached;
    const value = cs(el).position === "fixed" || hasFixedContext(el.parentElement);
    fixedContextCache.set(el, value);
    return value;
  };

  const reachableByInnerScroll = (el, axis) => {
    if (cs(el).position === 'fixed') return false;
    const rect = nowRect(el);
    const vertical = axis === 'y';
    let node = el.parentElement;
    while (node && node !== document.documentElement) {
      const style = cs(node);
      const overflow = vertical ? style.overflowY : style.overflowX;
      const size = vertical ? node.clientHeight : node.clientWidth;
      const extent = vertical ? node.scrollHeight : node.scrollWidth;
      if (['auto', 'scroll', 'overlay'].includes(overflow) && extent > size + 2) {
        const box = nowRect(node);
        const start = (vertical ? box.top + node.clientTop : box.left + node.clientLeft);
        const viewportSize = vertical ? innerHeight : innerWidth;
        const position = vertical ? node.scrollTop : node.scrollLeft;
        const contentStart = (vertical ? rect.top : rect.left) - start + position;
        const contentEnd = (vertical ? rect.bottom : rect.right) - start + position;
        if (start >= -2 && start + size <= viewportSize + 2 && contentStart >= -2 && contentEnd <= extent + 2) return true;
      }
      if (style.position === 'fixed') break;
      node = node.parentElement;
    }
    return false;
  };

  // Walks ancestors and reports content cut off by an ancestor's overflow
  // clipping. This is the common real-world crop: the element itself has
  // overflow visible, but a parent with overflow hidden/clip cuts it. Scrollable
  // ancestors on the cut axis count as a reachability path (not a defect), and
  // absolutely positioned elements skip ancestors outside their containing-block
  // chain because CSS does not clip them there.
  function ancestorClipReport(el) {
    const report = { scrollPathX: false, scrollPathY: false, cut: null };
    const elStyle = cs(el);
    if (elStyle.position === "fixed") return report;
    const rect = nowRect(el);
    // Content spills beyond the element's own box only when the element does not
    // clip/scroll that axis itself; a self-clipping element (ellipsis, hidden,
    // scrollable) is already handled by the self-overflow rule.
    const spillsX = elStyle.overflowX === "visible";
    const spillsY = elStyle.overflowY === "visible";
    const effRight = spillsX ? Math.max(rect.right, rect.left + (el.clientLeft || 0) + (el.scrollWidth || 0)) : rect.right;
    const effBottom = spillsY ? Math.max(rect.bottom, rect.top + (el.clientTop || 0) + (el.scrollHeight || 0)) : rect.bottom;
    const box = { left: rect.left, top: rect.top, right: effRight, bottom: effBottom };
    const width = Math.max(1, box.right - box.left);
    const height = Math.max(1, box.bottom - box.top);
    let carouselContext = CAROUSEL_TOKENS.test(markerText(el));
    let awaitingContainingBlock = elStyle.position === "absolute";
    let node = el.parentElement;
    while (node && node.nodeType === Node.ELEMENT_NODE) {
      const style = cs(node);
      if (!carouselContext && CAROUSEL_TOKENS.test(markerText(node))) carouselContext = true;
      const isContainingBlock = establishesContainingBlock(style);
      if (awaitingContainingBlock && !isContainingBlock) {
        node = node.parentElement;
        continue;
      }
      awaitingContainingBlock = false;
      const scrollableX = ["auto", "scroll", "overlay"].includes(style.overflowX);
      const scrollableY = ["auto", "scroll", "overlay"].includes(style.overflowY);
      const clipsX = ["hidden", "clip"].includes(style.overflowX) && !report.scrollPathX;
      const clipsY = ["hidden", "clip"].includes(style.overflowY) && !report.scrollPathY;
      if (clipsX || clipsY) {
        const nodeRect = nowRect(node);
        const clip = {
          left: nodeRect.left + (node.clientLeft || 0),
          top: nodeRect.top + (node.clientTop || 0),
        };
        clip.right = clip.left + node.clientWidth;
        clip.bottom = clip.top + node.clientHeight;
        const cutLeft = clipsX ? Math.max(0, clip.left - box.left) : 0;
        const cutRight = clipsX ? Math.max(0, box.right - clip.right) : 0;
        const cutTop = clipsY ? Math.max(0, clip.top - box.top) : 0;
        const cutBottom = clipsY ? Math.max(0, box.bottom - clip.bottom) : 0;
        const maxCut = Math.max(cutLeft, cutRight, cutTop, cutBottom);
        const visX = clipsX ? Math.max(0, Math.min(box.right, clip.right) - Math.max(box.left, clip.left)) / width : 1;
        const visY = clipsY ? Math.max(0, Math.min(box.bottom, clip.bottom) - Math.max(box.top, clip.top)) / height : 1;
        const cutFraction = 1 - visX * visY;
        if (maxCut > 4 && cutFraction > 0.08) {
          report.cut = {
            clipperSelector: selectorPath(node),
            cutLeft: round(cutLeft),
            cutRight: round(cutRight),
            cutTop: round(cutTop),
            cutBottom: round(cutBottom),
            cutFraction: round(cutFraction),
            fullyHidden: visX <= 0 || visY <= 0,
            singleLineEllipsis:
              style.textOverflow === "ellipsis" && style.whiteSpace === "nowrap" && cutTop === 0 && cutBottom === 0,
            lineClamp: Boolean(style.webkitLineClamp && style.webkitLineClamp !== "none"),
            carouselContext,
            overflowX: style.overflowX,
            overflowY: style.overflowY,
          };
          return report;
        }
      }
      if (scrollableX) report.scrollPathX = true;
      if (scrollableY) report.scrollPathY = true;
      if (style.position === "fixed") break;
      if (style.position === "absolute") awaitingContainingBlock = true;
      node = node.parentElement;
    }
    return report;
  }

  for (const el of candidates) {
    const style = cs(el);
    const rect = nowRect(el);
    const text = textOf(el);
    const complexArtifact = complexArtifactContext(el);
    const isSingleLineEllipsis = style.textOverflow === "ellipsis" && style.whiteSpace === "nowrap";
    const lineClampAllowed = Boolean(style.webkitLineClamp && style.webkitLineClamp !== "none");
    const allowedTruncation = truncationReason(el);
    const meaningful = isControl(el) || isLeafText(el);
    if (meaningful && text) {
      const clipsX = ["hidden", "clip"].includes(style.overflowX) && el.scrollWidth > el.clientWidth + 3;
      const clipsY = ["hidden", "clip"].includes(style.overflowY) && el.scrollHeight > el.clientHeight + 3;
      if ((clipsX || clipsY) && complexArtifact) {
        add("warning", "complex-artifact-overflow", el, "Complex map/chart/media internals are scroll-clipped; review visually if this artifact is the primary content.", {
          evidence: {
            overflowX: style.overflowX,
            overflowY: style.overflowY,
            scrollWidth: el.scrollWidth,
            clientWidth: el.clientWidth,
            scrollHeight: el.scrollHeight,
            clientHeight: el.clientHeight,
          },
        });
      } else if ((clipsX || clipsY) && !(allowedTruncation || (!config.rules.strictTruncation && (isSingleLineEllipsis || lineClampAllowed)))) {
        add("critical", clipsX ? "clipped-x" : "clipped-y", el, "Visible text/control content is clipped without an explicit allowance.", {
          evidence: {
            overflowX: style.overflowX,
            overflowY: style.overflowY,
            scrollWidth: el.scrollWidth,
            clientWidth: el.clientWidth,
            scrollHeight: el.scrollHeight,
            clientHeight: el.clientHeight,
          },
        });
      } else if ((clipsX || clipsY) && allowedTruncation) {
        add("warning", "allowed-truncation", el, "Content is clipped but has an explicit truncation allowance.", {
          evidence: { reason: allowedTruncation },
        });
      } else if ((clipsX || clipsY) && ellipsisTruncations.length < 100) {
        ellipsisTruncations.push({
          selector: selectorPath(el),
          textSnippet: snippet(el),
          kind: isSingleLineEllipsis ? "text-overflow-ellipsis" : "line-clamp",
        });
      }
    }

    // Ancestor clipping: the element itself may not clip, but a parent's
    // overflow hidden/clip can still cut it (the most common crop mechanism).
    let geo = null;
    if (meaningful && (text || isControl(el))) {
      geo = ancestorClipReport(el);
      const cut = geo.cut;
      if (cut) {
        const evidence = { evidence: cut };
        if (cut.singleLineEllipsis && !config.rules.strictTruncation && cut.cutTop === 0 && cut.cutBottom === 0) {
          if (ellipsisTruncations.length < 100) {
            ellipsisTruncations.push({ selector: selectorPath(el), textSnippet: snippet(el), kind: "ancestor-text-overflow-ellipsis" });
          }
        } else if (cut.lineClamp && !config.rules.strictTruncation) {
          if (ellipsisTruncations.length < 100) {
            ellipsisTruncations.push({ selector: selectorPath(el), textSnippet: snippet(el), kind: "ancestor-line-clamp" });
          }
        } else if (allowedTruncation) {
          add("warning", "allowed-truncation", el, "Content is cut by an ancestor clip but has an explicit truncation allowance.", {
            evidence: { reason: allowedTruncation, ...cut },
          });
        } else if (complexArtifact) {
          add("warning", "complex-artifact-overflow", el, "Complex map/chart/media internals are cut by an ancestor clip; review visually if this artifact is the primary content.", evidence);
        } else if (cut.fullyHidden) {
          add("warning", "clipped-hidden", el, "Text/control is fully hidden by an ancestor's overflow clipping; verify this state is intentional.", evidence);
        } else if (cut.carouselContext) {
          add("warning", "clipped-by-ancestor", el, "Text/control is partially cut by an ancestor clip inside a carousel/slider context.", evidence);
        } else {
          add("critical", "clipped-by-ancestor", el, "Visible text/control is cut by an ancestor's overflow clipping without a scroll path or allowance.", evidence);
        }
      }
    }

    if (meaningful && !complexArtifact) {
      if (hasFixedContext(el)) {
        const reachableX = reachableByInnerScroll(el, 'x');
        const reachableY = reachableByInnerScroll(el, 'y');
        const cutLeft = reachableX ? 0 : Math.max(0, -rect.left);
        const cutRight = reachableX ? 0 : Math.max(0, rect.right - window.innerWidth);
        const cutTop = reachableY ? 0 : Math.max(0, -rect.top);
        const cutBottom = reachableY ? 0 : Math.max(0, rect.bottom - window.innerHeight);
        const visW = Math.max(0, Math.min(rect.right, window.innerWidth) - Math.max(rect.left, 0));
        const visH = Math.max(0, Math.min(rect.bottom, window.innerHeight) - Math.max(rect.top, 0));
        const maxCut = Math.max(cutLeft, cutRight, cutTop, cutBottom);
        const cutFraction = 1 - (visW * visH) / Math.max(1, rect.width * rect.height);
        if ((!reachableX && visW <= 0) || (!reachableY && visH <= 0)) {
          add("warning", "fixed-offscreen-hidden", el, "Fixed-position text/control is entirely outside the viewport and cannot be scrolled to; verify this state is intentional.", {
            evidence: { viewportWidth: window.innerWidth, viewportHeight: window.innerHeight },
          });
        } else if (maxCut > 4 && cutFraction > 0.08) {
          add("critical", "fixed-offscreen-cut", el, "Fixed-position text/control is partially cut by the viewport edge and cannot be scrolled into view.", {
            evidence: {
              cutLeft: round(cutLeft),
              cutRight: round(cutRight),
              cutTop: round(cutTop),
              cutBottom: round(cutBottom),
              viewportWidth: window.innerWidth,
              viewportHeight: window.innerHeight,
            },
          });
        }
      } else {
        const rtl = (document.documentElement.getAttribute("dir") || "").toLowerCase() === "rtl";
        const reachableX = reachableByInnerScroll(el, 'x');
        const reachableY = reachableByInnerScroll(el, 'y');
        const absLeft = rect.left + window.scrollX;
        const absTop = rect.top + window.scrollY;
        const cutLeft = rtl || reachableX ? 0 : Math.max(0, -absLeft);
        const cutTop = reachableY ? 0 : Math.max(0, -absTop);
        if ((cutLeft > 4 && cutLeft / Math.max(1, rect.width) > 0.08) || (cutTop > 4 && cutTop / Math.max(1, rect.height) > 0.08)) {
          const fullyOut = (!reachableX && !rtl && absLeft + rect.width <= 0) || (!reachableY && absTop + rect.height <= 0);
          if (fullyOut) {
            add("warning", "offcanvas-hidden", el, "Text/control is positioned entirely before the document origin (possible visually-hidden pattern); verify it is intentional.", {
              evidence: { documentLeft: round(absLeft), documentTop: round(absTop) },
            });
          } else {
            add("critical", "offcanvas-cut", el, "Text/control is partially cut by the document edge and cannot be scrolled into view.", {
              evidence: { documentLeft: round(absLeft), documentTop: round(absTop), cutLeft: round(cutLeft), cutTop: round(cutTop) },
            });
          }
        }
        if (isControl(el) && rect.left > window.innerWidth + 2 && !(geo && geo.scrollPathX)) {
          const reachableByDocScroll = rect.left + window.scrollX < doc.scrollWidth - 2;
          if (!reachableByDocScroll) {
            add("critical", "interactive-offscreen-x", el, "Interactive element is outside the horizontal viewport and beyond the document scroll range.", {
              evidence: { viewportWidth: window.innerWidth, documentScrollWidth: doc.scrollWidth },
            });
          }
        }
      }
    }

    for (const area of configuredAreaRoots) {
      if (area.el === el || area.el.contains(el)) {
        const areaRect = nowRect(area.el);
        if (rect.left < areaRect.left - 2 || rect.right > areaRect.right + 2 || rect.top < areaRect.top - 2 || rect.bottom > areaRect.bottom + 2) {
          add(meaningful ? "critical" : "warning", "outside-area", el, "Element is rendered outside its declared area of interest.", {
            area: area.name,
            evidence: { areaRect: rectObj(areaRect) },
          });
        }
      }
    }
    if (meaningful && text && !complexArtifact) {
      const contrast = contrastAgainstBackground(el);
      if (contrast.transparentText) {
        add("critical", "invisible-text", el, "Text is effectively transparent.", { evidence: contrast });
      } else if (contrast.unmeasurable) {
        // Effective background is a gradient/image or a translucent stack: contrast
        // against white would be a false positive, so never emit a critical here.
        if (unmeasurableContrast.length < 200) {
          unmeasurableContrast.push({
            selector: selectorPath(el),
            reason: contrast.unmeasurableReason || "unmeasurable-background",
            color: contrast.color,
            backgroundColor: contrast.backgroundColor,
          });
        }
        add("warning", "unmeasurable-contrast", el, "Text contrast could not be measured against a solid background; review visually.", { evidence: contrast });
      } else if (contrast.ratio !== null) {
        const style = cs(el);
        const fontSize = px(style.fontSize);
        const fontWeight = Number(style.fontWeight) || (String(style.fontWeight).toLowerCase() === "bold" ? 700 : 400);
        const largeText = fontSize >= 24 || (fontSize >= 18.66 && fontWeight >= 700);
        const requiredRatio = largeText ? 3 : 4.5;
        const disabled = Boolean(
          el.matches(":disabled,[aria-disabled='true']") ||
          el.closest(":disabled,[aria-disabled='true']")
        );
        const allowance = disabled ? "inactive component" : contrastReason(el);
        if (contrast.ratio < 1.15 && !allowance) {
          add("critical", "invisible-text", el, "Text foreground/background contrast is effectively invisible.", {
            evidence: { ...contrast, requiredRatio, largeText, fontSize, fontWeight },
          });
        } else if (contrast.ratio < requiredRatio) {
          if (allowance) {
            add("warning", "allowed-contrast", el, "Text is below its WCAG contrast threshold under an explicit documented exception.", {
              evidence: { ...contrast, requiredRatio, largeText, fontSize, fontWeight, reason: allowance },
            });
          } else {
            add("critical", "insufficient-text-contrast", el, "Text is below the WCAG 2.2 AA contrast threshold for its rendered size and weight.", {
              evidence: { ...contrast, requiredRatio, largeText, fontSize, fontWeight },
            });
          }
        }
      }
    }
    if (isControl(el) && !complexArtifact && rect.width * rect.height < 400) {
      add("warning", "tiny-interactive-target", el, "Interactive target is very small.", { evidence: { area: Math.round(rect.width * rect.height) } });
    }
  }

  // Media health runs over every img/video that participates in rendering,
  // regardless of rect size: a broken image usually collapses to ~0x0, which is
  // exactly why it must not be filtered out by the visibility size gate.
  for (const el of deepQueryAll("img,video")) {
    if (isIgnored(el)) continue;
    const style = cs(el);
    if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") continue;
    if (effectiveOpacity(el) <= 0.01) continue;
    if (el.localName === "img") {
      if (el.complete && el.naturalWidth === 0 && (el.currentSrc || el.getAttribute("src"))) {
        const rect = nowRect(el);
        add("critical", "broken-image", el, "Image failed to load.", {
          evidence: {
            currentSrc: el.currentSrc || el.src,
            rect: rectObj(rect),
            collapsed: rect.width <= 1 || rect.height <= 1,
          },
        });
      } else if (!el.complete) {
        pendingMedia += 1;
      }
    } else if (el.error) {
      add("critical", "broken-video", el, "Visible video has a media error.", { evidence: { code: el.error.code } });
    }
  }

  const occlusionCandidates = [
    ...candidates.filter((el) => isControl(el) && !overlapReason(el) && !complexArtifactContext(el)),
    ...candidates.filter((el) => !isControl(el) && isLeafText(el) && !overlapReason(el) && !complexArtifactContext(el)),
  ].slice(0, 400);
  const originalScroll = { x: window.scrollX, y: window.scrollY };
  const originalInnerScroll = allElements
    .filter((element) => element !== document.scrollingElement && (
      element.scrollLeft !== 0 || element.scrollTop !== 0 ||
      element.scrollWidth > element.clientWidth || element.scrollHeight > element.clientHeight
    ))
    .map((element) => ({ element, left: element.scrollLeft, top: element.scrollTop }));
  const inViewport = (rect) =>
    rect.bottom > 0 && rect.right > 0 && rect.top < window.innerHeight && rect.left < window.innerWidth;
  const occluderOpacity = (node) => {
    // Effective opacity of an occluder for the purpose of "does it hide content":
    // combine element opacity with its own background-color alpha. Low values mean
    // the covered content can plausibly still be seen through the occluder.
    let ancestorOpacity = 1;
    let walk = node;
    while (walk && walk.nodeType === Node.ELEMENT_NODE) {
      ancestorOpacity *= Number(getComputedStyle(walk).opacity || 1);
      walk = walk.parentElement;
    }
    const bg = parseCssColor(getComputedStyle(node).backgroundColor);
    const bgAlpha = bg ? bg.a : 0;
    return ancestorOpacity * bgAlpha;
  };
  const scrollAncestorClipBox = (element) => {
    // The box the element can actually paint in right now: the viewport
    // intersected with every scrollable ancestor's client box. Content outside
    // this box is reachable by scrolling that container (the same reachability
    // path ancestorClipReport honors), so occlusion sampling must not hit-test
    // document coordinates there — elementsFromPoint would blame whatever
    // legitimately paints in that space (e.g. a neighboring panel).
    const box = { left: 0, top: 0, right: window.innerWidth, bottom: window.innerHeight };
    if (cs(element).position === "fixed") return box;
    let anc = composedParent(element);
    while (anc && anc.nodeType === Node.ELEMENT_NODE) {
      if (anc === document.body || anc === document.documentElement) break;
      const style = cs(anc);
      const scrollableX = ["auto", "scroll", "overlay"].includes(style.overflowX);
      const scrollableY = ["auto", "scroll", "overlay"].includes(style.overflowY);
      if (scrollableX || scrollableY) {
        const ancRect = anc.getBoundingClientRect();
        if (scrollableX) {
          box.left = Math.max(box.left, ancRect.left + (anc.clientLeft || 0));
          box.right = Math.min(box.right, ancRect.left + (anc.clientLeft || 0) + anc.clientWidth);
        }
        if (scrollableY) {
          box.top = Math.max(box.top, ancRect.top + (anc.clientTop || 0));
          box.bottom = Math.min(box.bottom, ancRect.top + (anc.clientTop || 0) + anc.clientHeight);
        }
      }
      if (style.position === "fixed") break;
      anc = composedParent(anc);
    }
    return box;
  };
  const restoreScrollPositions = () => {
    for (const { element, left, top } of originalInnerScroll) {
      if (element.isConnected && (element.scrollLeft !== left || element.scrollTop !== top)) {
        element.scrollTo({ left, top, behavior: "instant" });
      }
    }
    if (window.scrollX !== originalScroll.x || window.scrollY !== originalScroll.y) {
      window.scrollTo({ left: originalScroll.x, top: originalScroll.y, behavior: "instant" });
    }
  };
  const topAtPoint = (element, point) => {
    const root = element.getRootNode?.() || document;
    const hitTestRoot = typeof root.elementsFromPoint === "function" ? root : document;
    return hitTestRoot.elementsFromPoint(point.x, point.y).find((node) =>
      node.nodeType === Node.ELEMENT_NODE && !isIgnored(node) && cs(node).pointerEvents !== "none"
    );
  };
  const containsComposed = (ancestor, element) => {
    for (let current = element; current; current = composedParent(current)) {
      if (current === ancestor) return true;
    }
    return false;
  };
  const contextualCover = (element, occluder) => {
    for (let owner = occluder; owner && !containsComposed(owner, element); owner = composedParent(owner)) {
      const reason = owner.getAttribute?.("data-ui-contextual-overlay")?.trim();
      if (reason && owner.matches('[role="dialog"], [role="menu"], [role="listbox"], [role="tooltip"], [popover]') &&
          ["fixed", "absolute"].includes(cs(owner).position)) return { owner, reason };
    }
    return null;
  };
  const pinnedSibling = (element, occluder) => {
    for (let sticky = occluder; sticky && !containsComposed(sticky, element); sticky = composedParent(sticky)) {
      const style = cs(sticky);
      if (style.position === "fixed") return null;
      if (style.position !== "sticky") continue;
      let container = composedParent(sticky);
      while (container && ![cs(container).overflowX, cs(container).overflowY].some((overflow) =>
        ["auto", "scroll", "overlay", "hidden"].includes(overflow)
      )) container = composedParent(container);
      if (!container || container === document.scrollingElement || !containsComposed(container, element) ||
          !["auto", "scroll", "overlay"].includes(cs(container).overflowX) ||
          container.scrollWidth <= container.clientWidth + 2) return null;
      const containerRect = nowRect(container);
      const left = containerRect.left + container.clientLeft;
      const right = left + container.clientWidth;
      const stickyRect = nowRect(sticky);
      if (style.left !== "auto" && Math.abs(stickyRect.left - left) <= 2) {
        return { container, sticky, edge: "left" };
      }
      if (style.right !== "auto" && Math.abs(stickyRect.right - right) <= 2) {
        return { container, sticky, edge: "right" };
      }
      return null;
    }
    return null;
  };
  const reachablePastPinnedSibling = (element, pinned, point, rect) => {
    if (!pinned) return false;
    const { container, sticky, edge } = pinned;
    const clip = scrollAncestorClipBox(element);
    const stickyRect = nowRect(sticky);
    const left = edge === "left" ? Math.max(clip.left, stickyRect.right + 2) : clip.left;
    const right = edge === "right" ? Math.min(clip.right, stickyRect.left - 2) : clip.right;
    if (right - left <= 2) return false;
    const original = { left: container.scrollLeft, top: container.scrollTop };
    try {
      container.scrollTo({ left: original.left + point.x - (left + right) / 2, top: original.top, behavior: "instant" });
      if (container.scrollLeft === original.left) return false;
      const movedRect = nowRect(element);
      const movedPoint = { x: movedRect.left + point.x - rect.left, y: movedRect.top + point.y - rect.top };
      const movedClip = scrollAncestorClipBox(element);
      if (movedPoint.x < movedClip.left || movedPoint.x > movedClip.right ||
          movedPoint.y < movedClip.top || movedPoint.y > movedClip.bottom) return false;
      const top = topAtPoint(element, movedPoint);
      if (top && (top === element || element.contains(top) || top.contains(element))) return { visible: true };
      return top ? { contextual: contextualCover(element, top) } : null;
    } finally {
      container.scrollTo({ ...original, behavior: "instant" });
    }
  };
  const measureOcclusion = (el) => {
    if (!el.isConnected || !visible(el)) return;
    let measuredAfterScroll = false;
    if (!inViewport(nowRect(el))) {
      // Only elements outside the current viewport may be scrolled into view; those
      // are flagged so the finding reflects "occluded when scrolled to" not "as seen".
      el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
      measuredAfterScroll = true;
    }
    let rect = nowRect(el);
    if (rect.width <= 1 || rect.height <= 1) return;
    // Scroll-container reachability: an element scrolled out of an inner
    // overflow container can still sit inside the window viewport. Scroll it
    // into view within its container first (mirroring the window case above);
    // hit-testing where it is clipped away would report a false occlusion.
    let clip = scrollAncestorClipBox(el);
    if (
      Math.min(rect.right, clip.right) - Math.max(rect.left, clip.left) <= 2 ||
      Math.min(rect.bottom, clip.bottom) - Math.max(rect.top, clip.top) <= 2
    ) {
      el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
      measuredAfterScroll = true;
      rect = nowRect(el);
      clip = scrollAncestorClipBox(el);
      if (rect.width <= 1 || rect.height <= 1) return;
    }
    const insetX = Math.min(8, Math.max(2, rect.width / 4));
    const insetY = Math.min(8, Math.max(2, rect.height / 4));
    const sampleLeft = Math.max(0, clip.left);
    const sampleTop = Math.max(0, clip.top);
    const sampleRight = Math.min(window.innerWidth, clip.right);
    const sampleBottom = Math.min(window.innerHeight, clip.bottom);
    const points = [
      { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 },
      { x: rect.left + insetX, y: rect.top + insetY },
      { x: rect.right - insetX, y: rect.top + insetY },
      { x: rect.left + insetX, y: rect.bottom - insetY },
      { x: rect.right - insetX, y: rect.bottom - insetY },
    ].filter((point) => point.x >= sampleLeft && point.y >= sampleTop && point.x <= sampleRight && point.y <= sampleBottom);
    if (points.length < 2) return;
    let covered = 0;
    const contextualCovers = new Map();
    let maxOccluderOpacity = 0;
    const evidencePoints = [];
    for (const point of points) {
      const top = topAtPoint(el, point);
      const ok = top && (top === el || el.contains(top) || top.contains(el));
      const pinned = !ok && top ? pinnedSibling(el, top) : null;
      const probed = !ok ? reachablePastPinnedSibling(el, pinned, point, rect) : null;
      const reachableAfterScroll = probed?.visible === true;
      const contextual = (!ok && top ? contextualCover(el, top) : null) || probed?.contextual;
      if (contextual) contextualCovers.set(contextual.owner, contextual.reason);
      evidencePoints.push({
        x: round(point.x),
        y: round(point.y),
        topSelector: top ? selectorPath(top) : "",
        covered: !ok && !reachableAfterScroll && !contextual,
        ...(reachableAfterScroll ? { reachableAfterScroll: true } : {}),
        ...(contextual ? { contextualOverlay: selectorPath(contextual.owner) } : {}),
      });
      if (!ok && !reachableAfterScroll && !contextual) {
        covered += 1;
        if (top) maxOccluderOpacity = Math.max(maxOccluderOpacity, occluderOpacity(top));
        if (pinned) maxOccluderOpacity = Math.max(maxOccluderOpacity, occluderOpacity(pinned.sticky));
      }
    }
    if (contextualCovers.size) {
      add("warning", "allowed-contextual-overlay", el, "A declared temporary contextual surface covers this background item; its own content remains fully checked.", {
        evidence: { overlays: [...contextualCovers].map(([owner, reason]) => ({ selector: selectorPath(owner), reason })), samplePoints: evidencePoints, measuredAfterScroll },
      });
    }
    if (covered >= 2) {
      const coveredFraction = covered / points.length;
      // A translucent occluder may still leave the content legible: warn instead of fail.
      const lowOpacityOccluder = maxOccluderOpacity < 0.5;
      const evidence = {
        evidence: {
          samplePoints: evidencePoints,
          measuredAfterScroll,
          occluderOpacity: round(maxOccluderOpacity),
          coveredFraction: round(coveredFraction),
        },
      };
      if (covered === points.length) {
        add(lowOpacityOccluder ? "warning" : "critical", "occluded", el, "Meaningful text/control appears fully covered by an unrelated element.", evidence);
      } else if (coveredFraction >= 0.6) {
        add(lowOpacityOccluder ? "warning" : "critical", "partially-occluded", el, "Meaningful text/control is substantially covered by an unrelated element.", evidence);
      } else {
        add("warning", "partially-occluded", el, "Meaningful text/control is partially covered by an unrelated element.", evidence);
      }
    }
  };
  for (const element of occlusionCandidates) {
    try {
      restoreScrollPositions();
      measureOcclusion(element);
    } finally {
      restoreScrollPositions();
    }
  }

  function parseCssColor(value) {
    const raw = String(value || "").trim();
    if (!raw || raw === "transparent") return { r: 0, g: 0, b: 0, a: 0, raw };
    const match = /^rgba?\(([^)]+)\)$/.exec(raw);
    if (match) {
      const parts = match[1]
        .replace(/\//g, " ")
        .split(/[,\s]+/)
        .map((part) => part.trim())
        .filter(Boolean);
      if (parts.length < 3) return null;
      return {
        r: cssRgbChannel(parts[0]),
        g: cssRgbChannel(parts[1]),
        b: cssRgbChannel(parts[2]),
        a: parts.length > 3 ? cssAlpha(parts[3]) : 1,
        raw,
      };
    }
    const labMatch = /^lab\(([^)]+)\)$/.exec(raw);
    if (labMatch) {
      const parsed = parseColorFunctionParts(labMatch[1]);
      if (parsed.channels.length < 3) return null;
      return { ...labToRgb(parsed.channels[0], parsed.channels[1], parsed.channels[2]), a: parsed.alpha, raw };
    }
    const oklabMatch = /^oklab\(([^)]+)\)$/.exec(raw);
    if (oklabMatch) {
      const parsed = parseColorFunctionParts(oklabMatch[1]);
      if (parsed.channels.length < 3) return null;
      const lightness = parsed.rawChannels[0]?.endsWith("%") ? parsed.channels[0] / 100 : parsed.channels[0];
      return { ...oklabToRgb(lightness, parsed.channels[1], parsed.channels[2]), a: parsed.alpha, raw };
    }
    return null;
  }
  function parseColorFunctionParts(body) {
    const [channelsRaw, alphaRaw] = body.split("/");
    const rawChannels = channelsRaw
      .replace(/\//g, " ")
      .split(/[,\s]+/)
      .map((part) => part.trim())
      .filter(Boolean);
    return {
      rawChannels,
      channels: rawChannels.map(cssNumber),
      alpha: alphaRaw === undefined ? 1 : cssAlpha(alphaRaw.trim()),
    };
  }
  function cssNumber(value) {
    if (value === "none") return 0;
    if (value.endsWith("%")) return Number(value.slice(0, -1));
    return Number(value);
  }
  function cssAlpha(value) {
    if (value === "none") return 1;
    if (value.endsWith("%")) return clamp(Number(value.slice(0, -1)) / 100, 0, 1);
    return clamp(Number(value), 0, 1);
  }
  function cssRgbChannel(value) {
    if (value.endsWith("%")) return clamp(Number(value.slice(0, -1)) * 2.55, 0, 255);
    return clamp(Number(value), 0, 255);
  }
  function clamp(value, min, max) {
    if (!Number.isFinite(value)) return min;
    return Math.max(min, Math.min(max, value));
  }
  function linearToSrgb(value) {
    const clamped = clamp(value, 0, 1);
    return (clamped <= 0.0031308 ? clamped * 12.92 : 1.055 * clamped ** (1 / 2.4) - 0.055) * 255;
  }
  function labToRgb(lightness, a, b) {
    const fy = (lightness + 16) / 116;
    const fx = fy + a / 500;
    const fz = fy - b / 200;
    const epsilon = 216 / 24389;
    const kappa = 24389 / 27;
    const inverse = (value) => {
      const cubed = value ** 3;
      return cubed > epsilon ? cubed : (116 * value - 16) / kappa;
    };
    const x = 0.96422 * inverse(fx);
    const y = inverse(fy);
    const z = 0.82521 * inverse(fz);
    return {
      r: linearToSrgb(3.1338561 * x - 1.6168667 * y - 0.4906146 * z),
      g: linearToSrgb(-0.9787684 * x + 1.9161415 * y + 0.033454 * z),
      b: linearToSrgb(0.0719453 * x - 0.2289914 * y + 1.4052427 * z),
    };
  }
  function oklabToRgb(lightness, a, b) {
    const l1 = lightness + 0.3963377774 * a + 0.2158037573 * b;
    const m1 = lightness - 0.1055613458 * a - 0.0638541728 * b;
    const s1 = lightness - 0.0894841775 * a - 1.291485548 * b;
    const l = l1 ** 3;
    const m = m1 ** 3;
    const s = s1 ** 3;
    return {
      r: linearToSrgb(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
      g: linearToSrgb(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
      b: linearToSrgb(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
    };
  }
  function srgbToLinear(value) {
    const channel = clamp(value / 255, 0, 1);
    return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
  }
  function rgbToOklab(color) {
    const red = srgbToLinear(color.r);
    const green = srgbToLinear(color.g);
    const blue = srgbToLinear(color.b);
    const l = 0.4122214708 * red + 0.5363325363 * green + 0.0514459929 * blue;
    const m = 0.2119034982 * red + 0.6806995451 * green + 0.1073969566 * blue;
    const s = 0.0883024619 * red + 0.2817188376 * green + 0.6299787005 * blue;
    const lRoot = Math.cbrt(l);
    const mRoot = Math.cbrt(m);
    const sRoot = Math.cbrt(s);
    return {
      lightness: 0.2104542553 * lRoot + 0.793617785 * mRoot - 0.0040720468 * sRoot,
      a: 1.9779984951 * lRoot - 2.428592205 * mRoot + 0.4505937099 * sRoot,
      b: 0.0259040371 * lRoot + 0.7827717662 * mRoot - 0.808675766 * sRoot,
    };
  }
  function oklabToOklch(color) {
    const chroma = Math.sqrt(color.a ** 2 + color.b ** 2);
    let hue = Math.atan2(color.b, color.a) * 180 / Math.PI;
    if (hue < 0) hue += 360;
    return { lightness: color.lightness, chroma, hue };
  }
  function sampleThemePalette() {
    const grid = 24;
    const total = grid * grid;
    const samples = [];
    let unmeasurable = 0;
    let exceptions = 0;
    const hueBins = new Map();
    for (let row = 0; row < grid; row += 1) {
      for (let column = 0; column < grid; column += 1) {
        const x = (column + 0.5) * window.innerWidth / grid;
        const y = (row + 0.5) * window.innerHeight / grid;
        const stack = document.elementsFromPoint(x, y).filter((element) => !isIgnored(element));
        const element = stack.find((candidate) => visible(candidate));
        if (!element) {
          unmeasurable += 1;
          continue;
        }
        if (themeExceptionReason(element)) {
          exceptions += 1;
          continue;
        }
        if (element.closest("img,video,canvas,svg,picture")) {
          unmeasurable += 1;
          continue;
        }
        const background = backgroundFor(element);
        if (background.unmeasurable) {
          unmeasurable += 1;
          continue;
        }
        const color = oklabToOklch(rgbToOklab(background));
        samples.push(color);
        if (color.chroma >= 0.12) {
          const bin = Math.floor(((color.hue + 22.5) % 360) / 45);
          hueBins.set(bin, (hueBins.get(bin) || 0) + 1);
        }
      }
    }
    const measurable = samples.length;
    const opposite = samples.filter((sample) =>
      config.theme === "dark" ? sample.lightness >= 0.85 :
      (config.theme === "light" ? sample.lightness <= 0.25 : false)
    ).length;
    const highChroma = samples.filter((sample) => sample.chroma >= 0.12).length;
    const oppositeMeasurableFraction = measurable ? opposite / measurable : 0;
    const oppositeViewportFraction = opposite / total;
    const highChromaFraction = measurable ? highChroma / measurable : 0;
    const prominentHueClusters = [...hueBins.entries()]
      .filter(([, count]) => count / total >= 0.03)
      .map(([bin, count]) => ({
        hueCenter: bin * 45,
        viewportFraction: round(count / total),
      }));
    const metrics = {
      declaredTheme: config.theme,
      grid: `${grid}x${grid}`,
      totalSamples: total,
      measurableSamples: measurable,
      unmeasurableSamples: unmeasurable,
      exceptionSamples: exceptions,
      oppositeThemeSamples: opposite,
      oppositeMeasurableFraction: round(oppositeMeasurableFraction),
      oppositeViewportFraction: round(oppositeViewportFraction),
      highChromaFraction: round(highChromaFraction),
      prominentHueClusters,
    };
    if (config.theme !== "mixed") {
      if (oppositeMeasurableFraction >= 0.35 && oppositeViewportFraction >= 0.20) {
        add("critical", "declared-theme-contradiction", null, "A large share of the viewport contradicts the target's declared light or dark theme.", {
          evidence: metrics,
        });
      } else if (oppositeViewportFraction >= 0.15) {
        add("warning", "declared-theme-balance-risk", null, "A notable share of the viewport uses opposite-theme brightness and needs visual review.", {
          evidence: metrics,
        });
      }
    }
    if (highChromaFraction >= 0.25) {
      add("warning", "high-chroma-surface-risk", null, "High-chroma color occupies a large share of measurable surfaces; review palette restraint and intent.", {
        evidence: metrics,
      });
    }
    if (prominentHueClusters.length >= 4) {
      add("warning", "competing-accent-hues", null, "Four or more prominent accent-hue clusters compete in the viewport; review palette cohesion.", {
        evidence: metrics,
      });
    }
    if (unmeasurable / total >= 0.20) {
      add("warning", "unmeasurable-theme-surface", null, "A substantial part of the viewport uses media, gradients, or compositing that requires screenshot review.", {
        evidence: metrics,
      });
    }
    return metrics;
  }
  function blended(color, background) {
    const foregroundAlpha = Number.isFinite(color.a) ? color.a : 1;
    const backgroundAlpha = Number.isFinite(background.a) ? background.a : 1;
    const remainingAlpha = backgroundAlpha * (1 - foregroundAlpha);
    const alpha = foregroundAlpha + remainingAlpha;
    if (alpha === 0) return { r: 0, g: 0, b: 0, a: 0 };
    return {
      r: (color.r * foregroundAlpha + background.r * remainingAlpha) / alpha,
      g: (color.g * foregroundAlpha + background.g * remainingAlpha) / alpha,
      b: (color.b * foregroundAlpha + background.b * remainingAlpha) / alpha,
      a: alpha,
    };
  }
  function luminance(color) {
    const channel = (value) => {
      const n = value / 255;
      return n <= 0.03928 ? n / 12.92 : ((n + 0.055) / 1.055) ** 2.4;
    };
    return 0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b);
  }
  function contrastRatio(a, b) {
    const l1 = luminance(a);
    const l2 = luminance(b);
    const light = Math.max(l1, l2);
    const dark = Math.min(l1, l2);
    return (light + 0.05) / (dark + 0.05);
  }
  // Walk ancestors accumulating solid background layers. Returns a resolved opaque
  // background color only when the chain up to the first opaque layer is a genuine
  // stack of solid colors. If any ancestor in that chain paints a non-'none'
  // background-image (gradient/image) or the accumulated alpha never reaches opaque,
  // the effective background cannot be measured against white: return unmeasurable.
  function backgroundFor(el) {
    let node = el;
    let accum = null; // color painted so far, over an unknown backdrop
    while (node && node.nodeType === Node.ELEMENT_NODE) {
      const style = getComputedStyle(node);
      if (style.backgroundImage && style.backgroundImage !== "none") {
        return { unmeasurable: true, reason: "background-image/gradient", raw: style.backgroundImage };
      }
      const bg = parseCssColor(style.backgroundColor);
      if (bg && bg.a > 0.001) {
        if (bg.a >= 0.999) {
          const solid = accum ? blended(accum, bg) : bg;
          return { r: solid.r, g: solid.g, b: solid.b, a: 1, raw: bg.raw };
        }
        // Semi-transparent layer: composite over whatever accumulates below it.
        accum = accum ? blended(accum, bg) : { ...bg };
      }
      node = node.parentElement;
    }
    // Reached the root without an opaque solid backdrop.
    return { unmeasurable: true, reason: "translucent-stack", raw: accum ? accum.raw || "translucent" : "no-solid-background" };
  }
  function contrastAgainstBackground(el) {
    const fg = parseCssColor(getComputedStyle(el).color);
    if (!fg) {
      return { ratio: null, color: getComputedStyle(el).color, backgroundColor: "unparsed", transparentText: false };
    }
    const bg = backgroundFor(el);
    if (fg.a < 0.05) {
      return { ratio: null, color: fg.raw, backgroundColor: bg.raw, transparentText: true };
    }
    if (bg.unmeasurable) {
      return {
        ratio: null,
        color: fg.raw,
        backgroundColor: bg.raw,
        transparentText: false,
        unmeasurable: true,
        unmeasurableReason: bg.reason,
      };
    }
    const effectiveFg = fg.a < 1 ? blended(fg, bg) : fg;
    return {
      ratio: contrastRatio(effectiveFg, bg),
      color: fg.raw,
      backgroundColor: bg.raw,
      transparentText: false,
    };
  }
  function scrollbarVisibleForAxis(el, axis, style) {
    const overflow = axis === "x" ? style.overflowX : style.overflowY;
    const canScroll = axis === "x" ? el.scrollWidth > el.clientWidth + 2 : el.scrollHeight > el.clientHeight + 2;
    return overflow === "scroll" || (canScroll && ["auto", "overlay"].includes(overflow));
  }
  function documentScrollbarVisibleForAxis(el, axis, style) {
    const overflow = axis === "x" ? style.overflowX : style.overflowY;
    const canScroll = axis === "x" ? el.scrollWidth > el.clientWidth + 2 : el.scrollHeight > el.clientHeight + 2;
    if (["hidden", "clip"].includes(overflow)) return false;
    return overflow === "scroll" || canScroll;
  }
  function collectVisibleScrollbars() {
    const scrollbars = [];
    const scrolling = document.scrollingElement || document.documentElement;
    const scrollingStyle = getComputedStyle(scrolling);
    const docRect = { x: 0, y: 0, width: window.innerWidth, height: window.innerHeight, right: window.innerWidth, bottom: window.innerHeight };
    const documentActive = {
      x: documentScrollbarVisibleForAxis(scrolling, "x", scrollingStyle),
      y: documentScrollbarVisibleForAxis(scrolling, "y", scrollingStyle),
    };
    const activeByAxis = { x: new WeakSet(), y: new WeakSet() };
    const elementEntries = [];
    for (const el of allElements) {
      if (el === scrolling || isIgnored(el) || !visible(el)) continue;
      const style = getComputedStyle(el);
      const hasX = scrollbarVisibleForAxis(el, "x", style);
      const hasY = scrollbarVisibleForAxis(el, "y", style);
      if (!hasX && !hasY) continue;
      if (hasX) activeByAxis.x.add(el);
      if (hasY) activeByAxis.y.add(el);
      elementEntries.push({ el, style, hasX, hasY });
    }
    const sameAxisChain = (el, axis) => {
      const ancestors = [];
      let ancestor = composedParent(el);
      while (ancestor) {
        if (ancestor !== scrolling && activeByAxis[axis].has(ancestor)) {
          ancestors.unshift(selectorPath(ancestor));
        }
        ancestor = composedParent(ancestor);
      }
      if (documentActive[axis]) ancestors.unshift("document.scrollingElement");
      return [...ancestors, selectorPath(el)];
    };
    if (documentActive.x) {
      scrollbars.push({
        element: scrolling,
        selector: "document.scrollingElement",
        axis: "x",
        sameAxisDepth: 1,
        scrollChain: ["document.scrollingElement"],
        rect: docRect,
        scrollWidth: scrolling.scrollWidth,
        clientWidth: scrolling.clientWidth,
        scrollHeight: scrolling.scrollHeight,
        clientHeight: scrolling.clientHeight,
        overflowX: scrollingStyle.overflowX,
        overflowY: scrollingStyle.overflowY,
      });
    }
    if (documentActive.y) {
      scrollbars.push({
        element: scrolling,
        selector: "document.scrollingElement",
        axis: "y",
        sameAxisDepth: 1,
        scrollChain: ["document.scrollingElement"],
        rect: docRect,
        scrollWidth: scrolling.scrollWidth,
        clientWidth: scrolling.clientWidth,
        scrollHeight: scrolling.scrollHeight,
        clientHeight: scrolling.clientHeight,
        overflowX: scrollingStyle.overflowX,
        overflowY: scrollingStyle.overflowY,
      });
    }
    for (const { el, style, hasX, hasY } of elementEntries) {
      const base = {
        element: el,
        selector: selectorPath(el),
        rect: rectObj(nowRect(el)),
        scrollWidth: el.scrollWidth,
        clientWidth: el.clientWidth,
        scrollHeight: el.scrollHeight,
        clientHeight: el.clientHeight,
        overflowX: style.overflowX,
        overflowY: style.overflowY,
      };
      if (hasX) {
        const scrollChain = sameAxisChain(el, "x");
        scrollbars.push({ ...base, axis: "x", sameAxisDepth: scrollChain.length, scrollChain });
      }
      if (hasY) {
        const scrollChain = sameAxisChain(el, "y");
        scrollbars.push({ ...base, axis: "y", sameAxisDepth: scrollChain.length, scrollChain });
      }
    }
    return scrollbars;
  }

  const themePalette = config.inspectThemePalette === false ? null : sampleThemePalette();
  const visibleScrollbars = [];
  for (const scrollbar of collectVisibleScrollbars()) {
    const { element, ...evidence } = scrollbar;
    visibleScrollbars.push(evidence);
    const findingContext = {
      selector: evidence.selector,
      rect: evidence.rect,
      redactText: true,
      evidence,
    };
    if (evidence.axis === "x") {
      add("warning", "horizontal-scrollbar", element, "An active horizontal scrollbar is exceptional and requires review.", findingContext);
      if (evidence.sameAxisDepth >= 2) {
        add("critical", "nested-horizontal-scrollbars", element, "This horizontal scrollbar is nested inside another active horizontal scroll path.", findingContext);
      }
    } else if (evidence.sameAxisDepth === 2) {
      add("warning", "double-nested-vertical-scrollbars", element, "This vertical scrollbar creates a second same-axis scroll layer and requires review.", findingContext);
    } else if (evidence.sameAxisDepth >= 3) {
      add("critical", "triple-nested-vertical-scrollbars", element, "This vertical scrollbar creates a third or deeper same-axis scroll layer.", findingContext);
    }
  }
  const suppressedTotal = Object.values(suppressed).reduce((total, count) => total + count, 0);
  if (suppressedTotal > 0) {
    findings.push({
      severity: "warning",
      rule: "findings-truncated",
      message: `${suppressedTotal} additional findings were suppressed after the per-rule cap of ${MAX_PER_RULE}; fix the reported instances and re-run.`,
      selector: "document",
      textSnippet: "",
      rect: null,
      area: null,
      evidence: { suppressed },
    });
  }
  return {
    title: document.title,
    url: location.href,
    viewport: { width: window.innerWidth, height: window.innerHeight },
    metrics: {
      candidateCount: candidates.length,
      findingCount: findings.length,
      visibleScrollbars,
      unmeasurableContrast,
      notInspected,
      ellipsisTruncations,
      controlTextMeasurements,
      controlContainmentMeasurements,
      contentInsetMeasurements,
      themePalette,
      hiddenTextLike,
      pendingMedia,
      suppressedFindings: suppressed,
      document: {
        scrollWidth: doc.scrollWidth,
        clientWidth: doc.clientWidth,
        scrollHeight: doc.scrollHeight,
        clientHeight: doc.clientHeight,
      },
    },
    findings,
  };
}

async function observeSourceBinding(page, response, binding) {
  if (!binding) {
    return {
      status: "unbound",
      expected: null,
      observed: null,
      observedFrom: null,
    };
  }
  let observed = null;
  let observedFrom = null;
  if (binding.responseHeader) {
    const headerValue = response?.headers()?.[binding.responseHeader];
    if (typeof headerValue === "string" && headerValue.trim()) {
      observed = headerValue.trim().slice(0, 512);
      observedFrom = `response-header:${binding.responseHeader}`;
    }
  }
  if (!observed && binding.metaName) {
    observed = await page.evaluate((metaName) => {
      for (const element of document.querySelectorAll("meta[name]")) {
        if (element.getAttribute("name") === metaName) {
          return (element.getAttribute("content") || "").trim().slice(0, 512) || null;
        }
      }
      return null;
    }, binding.metaName).catch(() => null);
    if (observed) observedFrom = `meta:${binding.metaName}`;
  }
  return {
    status: !observed ? "missing" : (observed === binding.expected ? "matched" : "mismatched"),
    expected: binding.expected,
    observed,
    observedFrom,
  };
}

function pngDimensions(buffer) {
  if (
    buffer.length < 24 ||
    buffer[0] !== 0x89 ||
    buffer.toString("ascii", 1, 4) !== "PNG"
  ) {
    throw new Error("captured screenshot is not a valid PNG");
  }
  return { width: buffer.readUInt32BE(16), height: buffer.readUInt32BE(20) };
}

function screenshotActionValues(target) {
  const values = [];
  for (const action of target.verificationState?.actions || []) {
    if (action.action !== "fill" || typeof action.value !== "string" || !action.value) continue;
    values.push(action.value);
  }
  return [...new Set(values)];
}

async function screenshotMasks(page, target, config) {
  const masks = [];
  for (const entry of [...config.screenshotMasks, ...(target.screenshotMasks || [])]) {
    const locator = page.locator(entry.selector);
    try {
      await locator.count();
    } catch (error) {
      throw new Error(`screenshot mask selector could not be evaluated (${error.name || "selector error"})`);
    }
    masks.push(locator);
  }
  for (const value of screenshotActionValues(target)) {
    masks.push(page.getByText(value, { exact: false }));
  }
  return masks;
}

function screenshotArtifactPath(config, cellId, target, viewport, kind) {
  return path.join(
    config.screenshotDir,
    `${cellId}-${sanitizeFilePart(target.name || target.url)}-${sanitizeFilePart(viewport.name)}-${kind}.png`,
  );
}

async function captureEvidenceScreenshot(page, target, viewport, config, cellId, kind) {
  fs.mkdirSync(config.screenshotDir, { recursive: true, mode: 0o700 });
  const file = screenshotArtifactPath(config, cellId, target, viewport, kind);
  const masks = await screenshotMasks(page, target, config);
  const buffer = await page.screenshot({
    path: file,
    fullPage: kind === "full-page",
    animations: "disabled",
    caret: "hide",
    scale: "css",
    style: SCREENSHOT_REDACTION_STYLE,
    mask: masks,
    maskColor: "#777777",
  });
  const dimensions = pngDimensions(buffer);
  return {
    kind,
    path: file,
    mime: "image/png",
    size: buffer.length,
    sha256: sha256(buffer),
    width: dimensions.width,
    height: dimensions.height,
    capturedAt: new Date().toISOString(),
  };
}

function isLocalServerUrl(value) {
  try {
    const hostname = new URL(value).hostname.toLowerCase();
    return hostname === "localhost" || hostname.endsWith(".localhost") ||
      hostname === "127.0.0.1" || hostname === "::1";
  } catch {
    return false;
  }
}

function performanceThresholdStatus(value, threshold, assessed = true) {
  if (!assessed) return "not-applicable";
  if (!Number.isFinite(value)) return "unavailable";
  return value < threshold ? "pass" : "fail";
}

async function installRenderedPerformanceObserver(page) {
  await page.addInitScript(() => {
    const state = {
      lcpSupported: false,
      lcp: null,
      entryCount: 0,
      observer: null,
    };
    Object.defineProperty(globalThis, "__FORMAL_WEB_UI_PERFORMANCE__", {
      value: state,
      configurable: true,
    });
    try {
      state.lcpSupported = Array.isArray(PerformanceObserver.supportedEntryTypes) &&
        PerformanceObserver.supportedEntryTypes.includes("largest-contentful-paint");
      if (!state.lcpSupported) return;
      state.observer = new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) {
          state.entryCount += 1;
          state.lcp = {
            startTime: entry.startTime,
            renderTime: entry.renderTime || 0,
            loadTime: entry.loadTime || 0,
            size: entry.size || 0,
          };
        }
      });
      state.observer.observe({ type: "largest-contentful-paint", buffered: true });
    } catch {
      state.lcpSupported = false;
      state.observer = null;
    }
  });
}

async function waitForLcpObserverDelivery(page) {
  await page.evaluate(async () => {
    const images = [...document.images].filter((image) => {
      const rect = image.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0;
    });
    const decodeVisibleImages = Promise.allSettled(images.map(async (image) => {
      if (!image.complete) {
        await new Promise((resolve) => {
          image.addEventListener("load", resolve, { once: true });
          image.addEventListener("error", resolve, { once: true });
        });
      }
      if (typeof image.decode === "function") await image.decode().catch(() => {});
    }));
    await Promise.race([
      decodeVisibleImages,
      new Promise((resolve) => setTimeout(resolve, 100)),
    ]);
    await new Promise((resolve) => {
      let frames = 0;
      let stableFrames = 0;
      let lastCount = -1;
      const check = () => {
        frames += 1;
        const count = globalThis.__FORMAL_WEB_UI_PERFORMANCE__?.entryCount || 0;
        if (count === lastCount) stableFrames += 1;
        else stableFrames = 0;
        lastCount = count;
        if (frames >= 3 && stableFrames >= 2) resolve();
        else requestAnimationFrame(check);
      };
      requestAnimationFrame(check);
    });
  });
}

async function assessRenderedPerformance(page, thresholds) {
  let loadState = "complete";
  try {
    await page.waitForLoadState("load", { timeout: 5000 });
  } catch {
    loadState = "deadline";
  }
  await waitForLcpObserverDelivery(page).catch(() => {});
  let observed = null;
  try {
    observed = await page.evaluate(() => {
      const navigation = performance.getEntriesByType("navigation")[0] || null;
      const state = globalThis.__FORMAL_WEB_UI_PERFORMANCE__ || null;
      const ttfb = navigation && Number.isFinite(navigation.responseStart) &&
        Number.isFinite(navigation.requestStart) && navigation.responseStart >= navigation.requestStart
        ? navigation.responseStart - navigation.requestStart
        : null;
      return {
        ttfb,
        navigationType: navigation?.type || null,
        lcpSupported: Boolean(state?.lcpSupported),
        lcp: state?.lcp || null,
      };
    });
  } catch {
    observed = null;
  }
  const roundMetric = (value) => Number.isFinite(value) ? Math.round(value * 100) / 100 : null;
  const localServer = isLocalServerUrl(page.url());
  const ttfbAssessed = !thresholds.ttfbLocalOnly || localServer;
  const ttfbValue = roundMetric(observed?.ttfb);
  const lcpValue = roundMetric(observed?.lcp?.startTime);
  const ttfbStatus = performanceThresholdStatus(ttfbValue, thresholds.ttfbMs, ttfbAssessed);
  const lcpStatus = performanceThresholdStatus(lcpValue, thresholds.lcpMs);
  const metrics = {
    scope: "final main document before verifier full-page scrolling; document navigation LCP, not post-interaction latency",
    loadState,
    localServer,
    ttfb: {
      valueMs: ttfbValue,
      thresholdMs: thresholds.ttfbMs,
      comparison: "<",
      assessed: ttfbAssessed,
      status: ttfbStatus,
      source: "PerformanceNavigationTiming.responseStart-requestStart",
      navigationType: observed?.navigationType || null,
      ...(ttfbAssessed ? {} : { reason: "default local-server-only scope" }),
    },
    lcp: {
      valueMs: lcpValue,
      thresholdMs: thresholds.lcpMs,
      comparison: "<",
      assessed: true,
      status: lcpStatus,
      source: "LargestContentfulPaint.startTime",
      supported: Boolean(observed?.lcpSupported),
      size: roundMetric(observed?.lcp?.size),
    },
  };
  const findings = [];
  if (ttfbStatus === "fail") {
    findings.push({
      severity: "critical",
      rule: "ttfb-above-threshold",
      message: `Navigation TTFB must be below ${thresholds.ttfbMs} ms.`,
      selector: "document",
      textSnippet: "",
      rect: null,
      area: null,
      evidence: { ...metrics.ttfb, localServer },
    });
  } else if (ttfbStatus === "unavailable") {
    findings.push({
      severity: "warning",
      rule: "performance-metric-unavailable",
      message: "Required navigation TTFB could not be measured.",
      selector: "document",
      textSnippet: "",
      rect: null,
      area: null,
      evidence: { metric: "TTFB", ...metrics.ttfb, localServer },
    });
  }
  if (lcpStatus === "fail") {
    findings.push({
      severity: "critical",
      rule: "lcp-above-threshold",
      message: `Largest Contentful Paint must be below ${thresholds.lcpMs} ms.`,
      selector: "document",
      textSnippet: "",
      rect: null,
      area: null,
      evidence: metrics.lcp,
    });
  } else if (lcpStatus === "unavailable") {
    findings.push({
      severity: "warning",
      rule: "performance-metric-unavailable",
      message: "Required Largest Contentful Paint could not be measured.",
      selector: "document",
      textSnippet: "",
      rect: null,
      area: null,
      evidence: { metric: "LCP", ...metrics.lcp },
    });
  }
  return { metrics, findings };
}

async function verifyTarget(page, target, viewport, config, cellId) {
  const cellStartedMs = Date.now();
  await installRenderedPerformanceObserver(page);
  await page.setViewportSize({ width: viewport.width, height: viewport.height });
  const requestedRoute = routeEvidence(target.url);
  const reviewCellKey = sha256(stableJson({
    target: target.name || target.url,
    requestedOrigin: requestedRoute.origin,
    requestedPath: requestedRoute.path,
    stateName: target.stateName || "base",
    viewport: { name: viewport.name, width: viewport.width, height: viewport.height },
  }));
  const result = {
    cellId,
    target: publicTarget(target),
    viewport: publicViewport(viewport),
    skipped: false,
    outcome: "pending",
    skipReason: null,
    url: target.url,
    requestedPath: requestedRoute.path,
    finalPath: null,
    requestedOrigin: requestedRoute.origin,
    finalOrigin: null,
    redirected: false,
    sourceBinding: {
      status: "unbound",
      expected: null,
      observed: null,
      observedFrom: null,
    },
    startedAt: new Date(cellStartedMs).toISOString(),
    endedAt: null,
    durationMs: null,
    status: null,
    contentType: null,
    title: "",
    metrics: {},
    findings: [],
    screenshot: null,
    screenshots: { viewport: null, fullPage: null },
    evidenceErrors: [],
    continuation: { checked: false, evidence: null },
    handoffs: [],
    waitEvidence: [],
    actionTimings: [],
    timings: { stages: [] },
    review: {
      reviewCellKey,
      sourceFingerprint: target.reviewEvidence?.fingerprint || null,
      intentFingerprint: target.intentFingerprint || null,
    },
  };
  const finish = () => {
    const endedMs = Date.now();
    result.endedAt = new Date(endedMs).toISOString();
    result.durationMs = Math.max(0, endedMs - cellStartedMs);
    result.timings.totalMs = result.durationMs;
    return result;
  };
  const stage = async (name, operation) => {
    const started = Date.now();
    try {
      const value = await operation();
      result.timings.stages.push({ name, durationMs: Date.now() - started, outcome: "completed" });
      return value;
    } catch (error) {
      result.timings.stages.push({ name, durationMs: Date.now() - started, outcome: "failed" });
      throw error;
    }
  };
  if (target.contractErrors?.length) {
    result.skipped = true;
    result.outcome = "journey_contract_error";
    result.skipReason = target.contractErrors.join("; ");
    return finish();
  }
  let response;
  const initialWait = mergeWaitFor(config.waitFor, target.waitFor);
  const initialArmed = armWaitFor(page, initialWait);
  try {
    response = await stage("navigation", () => page.goto(
      target.url,
      { waitUntil: "domcontentloaded", timeout: 15000 },
    ));
    result.waitEvidence.push(...await stage(
      "initial-readiness",
      () => applyWaitFor(page, initialWait, initialArmed),
    ));
  } catch (error) {
    result.skipped = true;
    result.outcome = "navigation_error";
    result.skipReason = `navigation-failed: ${error.message}`;
    const finalRoute = routeEvidence(page.url());
    result.finalPath = finalRoute.path || null;
    result.finalOrigin = finalRoute.origin || null;
    return finish();
  }
  result.status = response ? response.status() : null;
  result.contentType = response ? response.headers()["content-type"] || "" : "";
  const finalRoute = routeEvidence(page.url());
  result.finalPath = finalRoute.path;
  result.finalOrigin = finalRoute.origin;
  result.redirected = Boolean(response?.request()?.redirectedFrom()) ||
    result.requestedPath !== result.finalPath ||
    result.requestedOrigin !== result.finalOrigin;
  if (!response || result.status >= 400 || (result.contentType && !/html|xhtml/i.test(result.contentType))) {
    result.skipped = true;
    if (!response) {
      result.outcome = "navigation_error";
      result.skipReason = "navigation-no-response";
    } else if (result.status >= 400) {
      result.outcome = "http_error";
      result.skipReason = `non-success-status-${result.status}`;
    } else {
      result.outcome = "non_html";
      result.skipReason = `non-html-content-type-${result.contentType || "unknown"}`;
    }
    result.title = await page.title().catch(() => "");
    return finish();
  }
  if (result.requestedPath !== result.finalPath || result.requestedOrigin !== result.finalOrigin) {
    result.skipped = true;
    result.outcome = "route_mismatch";
    result.skipReason = `unexpected-final-route-${result.finalPath || "unknown"}`;
    result.title = await page.title().catch(() => "");
    return finish();
  }
  const binding = target.sourceBinding || config.sourceBinding;
  result.sourceBinding = await observeSourceBinding(page, response, binding);
  if (result.sourceBinding.status === "missing" || result.sourceBinding.status === "mismatched") {
    result.skipped = true;
    result.outcome = result.sourceBinding.status === "missing"
      ? "source_binding_missing"
      : "stale_deployment";
    result.skipReason = result.sourceBinding.status === "missing"
      ? "deployment-did-not-report-required-source-binding"
      : "deployment-source-binding-does-not-match-expected-source";
    result.title = await page.title().catch(() => "");
    return finish();
  }
  let stateExecution = {
    beforeContinuation: null,
    waitEvidence: [],
    actionTimings: [],
    handoff: null,
    failure: null,
  };
  if (target.verificationState) {
    stateExecution = await stage(
      "interaction-state",
      () => applyInteractionState(page, target.verificationState, target),
    );
    result.waitEvidence.push(...stateExecution.waitEvidence);
    result.actionTimings.push(...stateExecution.actionTimings);
    if (stateExecution.handoff) result.handoffs.push(stateExecution.handoff);
    if (stateExecution.failure) {
      result.skipped = true;
      result.outcome = "interaction_error";
      result.skipReason = `interaction-state-${target.verificationState.name}-failed: ${stateExecution.failure.message}`;
      result.interactionFailure = stateExecution.failure;
      result.title = await page.title().catch(() => "");
      try {
        result.screenshots.viewport = await captureEvidenceScreenshot(
          page,
          target,
          viewport,
          config,
          cellId,
          "viewport",
        );
        result.screenshot = result.screenshots.viewport.path;
      } catch (error) {
        result.evidenceErrors.push(`failure viewport screenshot failed: ${error.message}`);
      }
      return finish();
    }
  }
  const stateRoute = routeEvidence(page.url());
  result.finalPath = stateRoute.path;
  result.finalOrigin = stateRoute.origin;
  result.redirected = result.redirected ||
    result.requestedPath !== result.finalPath ||
    result.requestedOrigin !== result.finalOrigin;
  const expectedStatePath = target.continuation?.kind === "navigation"
    ? target.continuation.expectedPath
    : result.requestedPath;
  if (expectedStatePath !== result.finalPath || result.requestedOrigin !== result.finalOrigin) {
    result.skipped = true;
    result.outcome = "route_mismatch";
    result.skipReason = `unexpected-final-route-${result.finalPath || "unknown"}; expected-${expectedStatePath || "unknown"}`;
    result.title = await page.title().catch(() => "");
    return finish();
  }
  if (target.continuation && !stateExecution.handoff) {
    const continuation = await verifyContinuation(
      page,
      target.continuation,
      stateExecution.beforeContinuation,
    );
    result.continuation = { checked: continuation.checked, evidence: continuation.evidence };
    result.findings.push(...continuation.findings);
  }
  const renderedPerformance = await stage(
    "rendered-performance",
    () => assessRenderedPerformance(page, target.performance),
  );
  result.metrics.performance = renderedPerformance.metrics;
  result.findings.push(...renderedPerformance.findings);
  const pageConfig = {
    areas: [...config.areas, ...(Array.isArray(target.areas) ? target.areas : [])],
    contentInsets: [...config.contentInsets, ...(Array.isArray(target.contentInsets) ? target.contentInsets : [])],
    ignore: [...config.ignore, ...normalizeTargetList(target.ignore)],
    allowTruncation: [...config.allowTruncation, ...normalizeTargetList(target.allowTruncation)],
    allowOverlap: [...config.allowOverlap, ...normalizeTargetList(target.allowOverlap)],
    allowContrast: [...config.allowContrast, ...normalizeTargetList(target.allowContrast)],
    themeExceptions: [...config.themeExceptions, ...normalizeTargetList(target.themeExceptions)],
    theme: target.theme,
    inspectThemePalette: true,
    rules: config.rules,
    // Playwright evaluates every reachable child frame separately below. This
    // prevents the top document from claiming that reachable iframe content
    // was ignored while still surfacing frames that detach or reject evaluation.
    inspectFramesExternally: true,
  };
  await page.evaluate((injected) => {
    window.__FORMAL_WEB_UI_CONFIG__ = injected;
  }, pageConfig);
  const journeyEvaluation = await page.evaluate(journeyHierarchyVerifier, {
    primaryJourney: target.primaryJourney,
    regions: target.regions,
  });
  result.metrics.journey = journeyEvaluation;
  result.findings.push(...journeyEvaluation.findings);
  try {
    result.screenshots.viewport = await captureEvidenceScreenshot(
      page,
      target,
      viewport,
      config,
      cellId,
      "viewport",
    );
    result.screenshot = result.screenshots.viewport.path;
  } catch (error) {
    result.evidenceErrors.push(`viewport screenshot failed: ${error.message}`);
  }
  let scrollMetrics = { skipped: true };
  if (config.scroll) {
    scrollMetrics = await scrollThroughPage(page).catch(() => ({ skipped: true, error: true }));
  }
  const evaluated = await page.evaluate(pageVerifier);
  const childFrames = page.frames().filter((frame) => frame !== page.mainFrame());
  const frameEvaluations = [];
  const frameFailures = [];
  for (const frame of childFrames) {
    const frameUrl = frame.url() || "about:blank";
    const frameName = frame.name() || "unnamed";
    try {
      await frame.evaluate((injected) => {
        window.__FORMAL_WEB_UI_CONFIG__ = injected;
      }, { ...pageConfig, inspectThemePalette: false });
      const frameResult = await frame.evaluate(pageVerifier);
      frameEvaluations.push({ frameUrl, frameName, result: frameResult });
    } catch (error) {
      frameFailures.push({ frameUrl, frameName, error: error.message });
    }
  }
  result.title = evaluated.title;
  result.outcome = "checked";
  result.url = evaluated.url;
  result.actualViewport = evaluated.viewport;
  const prefixSelector = (selector, frameUrl, frameName) =>
    `[frame ${frameName} ${frameUrl}] ${selector || "document"}`;
  const prefixScrollEvidence = (evidence, frameUrl, frameName) => ({
    ...(evidence || {}),
    ...(typeof evidence?.selector === "string"
      ? { selector: prefixSelector(evidence.selector, frameUrl, frameName) }
      : {}),
    ...(Array.isArray(evidence?.scrollChain)
      ? { scrollChain: evidence.scrollChain.map((selector) => prefixSelector(selector, frameUrl, frameName)) }
      : {}),
  });
  const mergedFindings = [...result.findings, ...evaluated.findings];
  const mergedMetrics = {
    ...evaluated.metrics,
    journey: journeyEvaluation,
    continuation: result.continuation,
    performance: result.metrics.performance,
    scroll: scrollMetrics,
    frames: [],
    frameDocuments: [],
  };
  const mergeCounts = (left = {}, right = {}) => {
    const merged = { ...left };
    for (const [key, value] of Object.entries(right || {})) {
      merged[key] = (merged[key] || 0) + (Number(value) || 0);
    }
    return merged;
  };
  for (const entry of frameEvaluations) {
    const frameResult = entry.result;
    mergedFindings.push(...frameResult.findings.map((finding) => ({
      ...finding,
      selector: prefixSelector(finding.selector, entry.frameUrl, entry.frameName),
      evidence: {
        ...prefixScrollEvidence(finding.evidence, entry.frameUrl, entry.frameName),
        frame: { url: entry.frameUrl, name: entry.frameName },
      },
    })));
    mergedMetrics.candidateCount = (mergedMetrics.candidateCount || 0) + (frameResult.metrics?.candidateCount || 0);
    mergedMetrics.visibleScrollbars = [
      ...(mergedMetrics.visibleScrollbars || []),
      ...(frameResult.metrics?.visibleScrollbars || []).map((scrollbar) => ({
        ...scrollbar,
        selector: prefixSelector(scrollbar.selector, entry.frameUrl, entry.frameName),
        scrollChain: (scrollbar.scrollChain || []).map((selector) =>
          prefixSelector(selector, entry.frameUrl, entry.frameName)
        ),
        frame: { url: entry.frameUrl, name: entry.frameName },
      })),
    ];
    mergedMetrics.unmeasurableContrast = [
      ...(mergedMetrics.unmeasurableContrast || []),
      ...(frameResult.metrics?.unmeasurableContrast || []).map((item) => ({
        ...item,
        selector: prefixSelector(item.selector, entry.frameUrl, entry.frameName),
        frame: { url: entry.frameUrl, name: entry.frameName },
      })),
    ];
    mergedMetrics.ellipsisTruncations = [
      ...(mergedMetrics.ellipsisTruncations || []),
      ...(frameResult.metrics?.ellipsisTruncations || []).map((item) => ({
        ...item,
        selector: prefixSelector(item.selector, entry.frameUrl, entry.frameName),
        frame: { url: entry.frameUrl, name: entry.frameName },
      })),
    ];
    mergedMetrics.controlTextMeasurements = [
      ...(mergedMetrics.controlTextMeasurements || []),
      ...(frameResult.metrics?.controlTextMeasurements || []).map((item) => ({
        ...item,
        selector: prefixSelector(item.selector, entry.frameUrl, entry.frameName),
        frame: { url: entry.frameUrl, name: entry.frameName },
      })),
    ];
    mergedMetrics.controlContainmentMeasurements = [
      ...(mergedMetrics.controlContainmentMeasurements || []),
      ...(frameResult.metrics?.controlContainmentMeasurements || []).map((item) => ({
        ...item,
        selector: prefixSelector(item.selector, entry.frameUrl, entry.frameName),
        ownerSelector: prefixSelector(item.ownerSelector, entry.frameUrl, entry.frameName),
        frame: { url: entry.frameUrl, name: entry.frameName },
      })),
    ];
    mergedMetrics.contentInsetMeasurements = [
      ...(mergedMetrics.contentInsetMeasurements || []),
      ...(frameResult.metrics?.contentInsetMeasurements || []).map((item) => ({
        ...item,
        selector: prefixSelector(item.selector, entry.frameUrl, entry.frameName),
        frame: { url: entry.frameUrl, name: entry.frameName },
      })),
    ];
    mergedMetrics.hiddenTextLike = mergeCounts(mergedMetrics.hiddenTextLike, frameResult.metrics?.hiddenTextLike);
    mergedMetrics.pendingMedia = (mergedMetrics.pendingMedia || 0) + (frameResult.metrics?.pendingMedia || 0);
    mergedMetrics.suppressedFindings = mergeCounts(mergedMetrics.suppressedFindings, frameResult.metrics?.suppressedFindings);
    mergedMetrics.frameDocuments.push({
      url: entry.frameUrl,
      name: entry.frameName,
      document: frameResult.metrics?.document || {},
    });
    mergedMetrics.frames.push({
      url: entry.frameUrl,
      name: entry.frameName,
      candidateCount: frameResult.metrics?.candidateCount || 0,
      findingCount: frameResult.findings.length,
      inspectedOpenShadowRoots: frameResult.metrics?.notInspected?.inspectedOpenShadowRoots || 0,
    });
  }
  for (const failure of frameFailures) {
    mergedFindings.push({
      severity: "warning",
      rule: "not-inspected",
      message: `Reachable iframe could not be inspected: ${failure.error}`,
      selector: prefixSelector("document", failure.frameUrl, failure.frameName),
      textSnippet: "",
      rect: null,
      area: null,
      evidence: { frame: { url: failure.frameUrl, name: failure.frameName }, error: failure.error },
    });
  }
  const shadowMetrics = [evaluated, ...frameEvaluations.map((entry) => entry.result)]
    .map((entry) => entry.metrics?.notInspected || {});
  mergedMetrics.notInspected = {
    openShadowRoots: shadowMetrics.reduce((sum, item) => sum + (item.openShadowRoots || 0), 0),
    iframes: frameFailures.length,
    inspectedOpenShadowRoots: shadowMetrics.reduce((sum, item) => sum + (item.inspectedOpenShadowRoots || 0), 0),
    discoveredOpenShadowRoots: shadowMetrics.reduce((sum, item) => sum + (item.discoveredOpenShadowRoots || 0), 0),
    inspectedIframes: frameEvaluations.length,
    discoveredIframes: Math.max(
      evaluated.metrics?.notInspected?.discoveredIframes || 0,
      childFrames.length,
    ),
  };
  mergedMetrics.findingCount = mergedFindings.length;
  result.metrics = mergedMetrics;
  result.findings = mergedFindings;
  try {
    result.screenshots.fullPage = await captureEvidenceScreenshot(
      page,
      target,
      viewport,
      config,
      cellId,
      "full-page",
    );
  } catch (error) {
    result.evidenceErrors.push(`full-page screenshot failed: ${error.message}`);
  }
  if (result.evidenceErrors.length) {
    result.skipped = true;
    result.outcome = "evidence_error";
    result.skipReason = result.evidenceErrors.join("; ");
  }
  return finish();
}

function normalizeTargetList(value) {
  if (!Array.isArray(value)) return [];
  return value.map((item) => typeof item === "string" ? { selector: item, reason: "target configured selector" } : item);
}

function summarizeFindings(pages) {
  const findings = [];
  for (const page of pages) {
    for (const finding of page.findings || []) {
      findings.push({
        ...finding,
        url: page.target.url,
        targetName: page.target.name || page.target.url,
        cellId: page.cellId,
        stateName: page.target.stateName || "base",
        viewport: page.viewport.name,
        requestedPath: page.requestedPath,
        finalPath: page.finalPath,
      });
    }
  }
  return findings;
}

function sampledWidthCoverage(cells) {
  const groups = new Map();
  for (const cell of cells) {
    const key = stableJson({
      targetName: cell.target.name || cell.target.url,
      requestedPath: cell.requestedPath,
      stateName: cell.target.stateName || "base",
    });
    if (!groups.has(key)) {
      groups.set(key, {
        targetName: cell.target.name || cell.target.url,
        requestedPath: cell.requestedPath,
        stateName: cell.target.stateName || "base",
        mode: "sampled-only",
        sampledWidths: [],
      });
    }
    groups.get(key).sampledWidths.push({
      viewport: cell.viewport.name,
      width: cell.viewport.width,
      height: cell.viewport.height,
      sampling: cell.viewport.sampling,
    });
  }
  return [...groups.values()];
}

function buildChangedReviewQueue(pages, config, runId) {
  const priorDecisions = new Map(
    (config.priorReview?.decisions || []).map((decision) => [decision.reviewCellKey, decision]),
  );
  const currentKeys = new Set();
  const cells = [];
  const entries = [];
  const blockingFindings = [];
  for (const page of pages) {
    if (page.outcome !== "checked" || !page.screenshots?.viewport || !page.screenshots?.fullPage) continue;
    const reviewCellKey = page.review.reviewCellKey;
    currentKeys.add(reviewCellKey);
    const prior = priorDecisions.get(reviewCellKey);
    const sourceFingerprint = page.review.sourceFingerprint;
    const intentFingerprint = page.review.intentFingerprint;
    const sourceUnchanged = Boolean(prior && prior.sourceFingerprint === sourceFingerprint);
    const intentUnchanged = Boolean(prior && prior.intentFingerprint === intentFingerprint);
    const screenshots = {
      viewport: page.screenshots.viewport,
      fullPage: page.screenshots.fullPage,
    };
    const common = {
      reviewCellKey,
      cellId: page.cellId,
      targetName: page.target.name || page.target.url,
      requestedPath: page.requestedPath,
      finalPath: page.finalPath,
      stateName: page.target.stateName || "base",
      viewport: page.viewport,
      primaryJourney: page.target.primaryJourney,
      theme: page.target.theme,
      sourceFingerprint,
      intentFingerprint,
      reviewInputs: page.target.reviewEvidence || null,
      screenshots,
      paletteRisks: (page.findings || [])
        .filter((finding) => [
          "declared-theme-balance-risk",
          "high-chroma-surface-risk",
          "competing-accent-hues",
          "unmeasurable-theme-surface",
        ].includes(finding.rule))
        .map((finding) => finding.rule),
    };
    if (prior && sourceUnchanged && intentUnchanged) {
      const cell = {
        ...common,
        status: prior.decision === "pass" ? "carried-pass" : `carried-${prior.decision}`,
        decision: prior.decision,
        note: prior.note || "",
        basis: "unchanged-ui-inputs-and-intent",
      };
      cells.push(cell);
      if (prior.decision !== "pass") {
        blockingFindings.push({
          severity: "critical",
          rule: "manual-review-gap-carried",
          message: "A prior manual visual-review gap remains blocking because its UI inputs and intent are unchanged.",
          selector: "document",
          textSnippet: "",
          rect: null,
          area: null,
          evidence: {
            reviewCellKey,
            priorDecision: prior.decision,
            note: prior.note,
            sourceFingerprint,
            intentFingerprint,
          },
          url: page.target.url,
          targetName: page.target.name || page.target.url,
          cellId: page.cellId,
          stateName: page.target.stateName || "base",
          viewport: page.viewport.name,
          requestedPath: page.requestedPath,
          finalPath: page.finalPath,
        });
      }
      continue;
    }
    const reasons = [];
    if (!prior) reasons.push("new-route-state-viewport");
    else {
      if (!sourceUnchanged) reasons.push("declared-ui-inputs-changed");
      if (!intentUnchanged) reasons.push("journey-or-theme-intent-changed");
    }
    const cell = { ...common, status: "review-required", decision: null, note: "", basis: reasons };
    cells.push(cell);
    entries.push({ ...common, reasons });
  }
  const disposed = new Map(config.reviewRemovedCells.map((item) => [item.reviewCellKey, item.reason]));
  const removed = config.development.enabled
    ? []
    : [...priorDecisions.keys()].filter((key) => !currentKeys.has(key)).sort();
  const undisposedRemoved = removed.filter((key) => !disposed.has(key));
  const invalidDispositions = [...disposed.keys()].filter((key) => !removed.includes(key)).sort();
  const removedDispositions = removed
    .filter((key) => disposed.has(key))
    .map((reviewCellKey) => ({ reviewCellKey, reason: disposed.get(reviewCellKey) }));
  const queue = {
    schemaVersion: REVIEW_QUEUE_SCHEMA_VERSION,
    kind: REVIEW_QUEUE_KIND,
    runId,
    generatedAt: new Date().toISOString(),
    priorReviewedRunId: config.priorReview?.reviewedRunId || null,
    trigger: "declared UI inputs, journey/theme intent, or new route/state/viewport; screenshot pixels are integrity-only",
    entries,
    carried: cells.filter((cell) => cell.status.startsWith("carried-")).map((cell) => ({
      reviewCellKey: cell.reviewCellKey,
      status: cell.status,
      decision: cell.decision,
      note: cell.note,
    })),
    removedDispositions,
    undisposedRemoved,
    invalidDispositions,
  };
  const coverageFailures = [
    ...undisposedRemoved.map((reviewCellKey) => ({
      reviewCellKey,
      reason: "previously reviewed cell is absent without an explicit reviewRemovedCells disposition",
    })),
    ...invalidDispositions.map((reviewCellKey) => ({
      reviewCellKey,
      reason: "reviewRemovedCells disposition does not reference a cell removed from the prior reviewed manifest",
    })),
  ];
  return {
    queue,
    blockingFindings,
    report: {
      priorReviewedRunId: config.priorReview?.reviewedRunId || null,
      priorManifestSha256: config.priorReview?.sha256 || null,
      pendingCount: entries.length,
      carriedPassCount: cells.filter((cell) => cell.status === "carried-pass").length,
      carriedGapCount: cells.filter((cell) => cell.status === "carried-gap").length,
      carriedBlockedCount: cells.filter((cell) => cell.status === "carried-blocked").length,
      cells,
      removedDispositions,
      coverageFailures,
    },
  };
}

function writeReviewQueueArtifact(queue, reviewQueueOut) {
  fs.mkdirSync(path.dirname(reviewQueueOut), { recursive: true });
  const bytes = Buffer.from(`${JSON.stringify(queue, null, 2)}\n`, "utf8");
  fs.writeFileSync(reviewQueueOut, bytes);
  return sha256(bytes);
}

function journeyScreenshotDescriptor(evidenceOut, screenshot) {
  if (!screenshot || typeof screenshot.path !== "string") return null;
  const root = path.dirname(evidenceOut);
  let absolute;
  let details;
  try {
    const original = fs.lstatSync(screenshot.path);
    if (!original.isFile() || original.isSymbolicLink()) return null;
    absolute = fs.realpathSync.native(screenshot.path);
    details = fs.lstatSync(absolute);
  } catch {
    return null;
  }
  if (!details.isFile() || details.isSymbolicLink() || !pathIsWithin(absolute, root)) {
    return null;
  }
  const relative = path.relative(root, absolute).split(path.sep).join("/");
  if (!relative || relative === ".." || relative.startsWith("../")) return null;
  return {
    kind: screenshot.kind,
    path: relative,
    mime: "image/png",
    size: details.size,
    sha256: screenshot.sha256,
    width: screenshot.width,
    height: screenshot.height,
    capturedAt: screenshot.capturedAt || null,
  };
}

function writeJourneyEvidenceArtifact(report, evidenceOut) {
  const reviewByCell = new Map(
    (report.review?.cells || []).map((cell) => [cell.reviewCellKey, cell]),
  );
  const cells = report.pages.map((page) => {
    const review = reviewByCell.get(page.review?.reviewCellKey);
    const screenshots = {
      viewport: journeyScreenshotDescriptor(evidenceOut, page.screenshots?.viewport),
      fullPage: journeyScreenshotDescriptor(evidenceOut, page.screenshots?.fullPage),
    };
    const findings = [];
    const seenFindings = new Set();
    for (const finding of page.findings || []) {
      const key = `${finding.severity}:${finding.rule}`;
      if (seenFindings.has(key) || findings.length >= 64) continue;
      seenFindings.add(key);
      findings.push({ severity: finding.severity, rule: finding.rule });
    }
    return {
      cellId: page.cellId,
      reviewCellKey: page.review?.reviewCellKey || null,
      planIndex: page.execution?.planIndex ?? null,
      targetName: page.target?.name || "Unnamed target",
      primaryJourney: page.target?.primaryJourney || null,
      stateName: page.target?.stateName || "base",
      requestedPath: page.requestedPath || null,
      finalPath: page.finalPath || null,
      viewport: page.viewport,
      startedAt: page.startedAt || null,
      endedAt: page.endedAt || null,
      durationMs: page.durationMs ?? null,
      outcome: page.outcome,
      httpStatus: page.status,
      sourceBindingStatus: page.sourceBinding?.status || "unbound",
      review: review ? {
        status: review.status,
        decision: review.decision || null,
      } : null,
      actions: (page.actionTimings || []).slice(0, 128).map((action) => ({
        index: action.index,
        action: action.action,
        outcome: action.outcome,
        durationMs: action.durationMs,
      })),
      findings,
      screenshots,
    };
  });
  const manifest = {
    schemaVersion: JOURNEY_EVIDENCE_SCHEMA_VERSION,
    kind: JOURNEY_EVIDENCE_KIND,
    runId: report.runId,
    governedRunId: process.env.DEVCOORDINATOR_RUN_ID || null,
    governedCheck: process.env.DEVCOORDINATOR_CHECK_NAME || null,
    generatedAt: report.generatedAt,
    browser: report.browser,
    coverage: {
      checkedPages: report.coverage?.checkedPages ?? 0,
      plannedPages: report.plan?.plannedPageCount ?? report.pages.length,
      failed: Boolean(report.coverage?.failed),
      readinessEligible: Boolean(report.coverage?.readinessEligible),
      requiredCoverage: report.coverage?.requiredCoverage || null,
    },
    cells,
  };
  fs.mkdirSync(path.dirname(evidenceOut), { recursive: true, mode: 0o700 });
  const bytes = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  fs.writeFileSync(evidenceOut, bytes, { mode: 0o600 });
  return {
    path: evidenceOut,
    sha256: sha256(bytes),
    size: bytes.length,
    cellCount: cells.length,
    screenshotCount: cells.reduce(
      (count, cell) => count + Number(Boolean(cell.screenshots.viewport)) + Number(Boolean(cell.screenshots.fullPage)),
      0,
    ),
  };
}

function governedEvidenceRoot() {
  const candidate = process.env.DEVCOORDINATOR_EVIDENCE_DIR;
  if (!candidate) return null;
  if (!path.isAbsolute(candidate)) {
    throw new Error("DEVCOORDINATOR_EVIDENCE_DIR must be absolute");
  }
  if (!process.env.DEVCOORDINATOR_RUN_ID || !process.env.DEVCOORDINATOR_CHECK_NAME) {
    throw new Error("governed visual evidence requires exact run and check identities");
  }
  fs.mkdirSync(candidate, { recursive: true, mode: 0o700 });
  const root = fs.realpathSync.native(candidate);
  fs.chmodSync(root, 0o700);
  return root;
}

function alreadyPublishedGovernedManifest(root, sourceManifest) {
  const relative = path.relative(root, sourceManifest).split(path.sep).join("/");
  if (relative === "journey-evidence.json") return true;
  return new RegExp(`^${GOVERNED_BUNDLE_DIRECTORY}/[0-9a-f]{64}/journey-evidence\\.json$`, "u")
    .test(relative);
}

function publishGovernedJourneyEvidenceArtifact(sourceManifest) {
  const root = governedEvidenceRoot();
  if (!root) return null;
  const source = fs.realpathSync.native(sourceManifest);
  if (alreadyPublishedGovernedManifest(root, source)) return null;
  const sourceRoot = fs.realpathSync.native(path.dirname(source));
  const manifest = JSON.parse(fs.readFileSync(source, "utf8"));
  if (
    manifest?.kind !== JOURNEY_EVIDENCE_KIND ||
    manifest?.governedRunId !== process.env.DEVCOORDINATOR_RUN_ID ||
    manifest?.governedCheck !== process.env.DEVCOORDINATOR_CHECK_NAME ||
    !Array.isArray(manifest?.cells)
  ) {
    throw new Error("custom-path journey evidence is not bound to this governed check");
  }
  const bundleKey = sha256(stableJson({
    governedRunId: manifest.governedRunId,
    governedCheck: manifest.governedCheck,
    formalRunId: manifest.runId,
  }));
  const bundles = path.join(root, GOVERNED_BUNDLE_DIRECTORY);
  fs.mkdirSync(bundles, { recursive: true, mode: 0o700 });
  const realBundles = fs.realpathSync.native(bundles);
  if (!pathIsWithin(realBundles, root)) {
    throw new Error("governed visual-evidence bundle directory escaped its test leaf");
  }
  const destination = path.join(realBundles, bundleKey);
  if (fs.existsSync(destination)) {
    throw new Error("governed visual-evidence bundle identity already exists");
  }
  const temporary = path.join(
    realBundles,
    `.tmp-${bundleKey}-${process.pid}-${randomBytes(4).toString("hex")}`,
  );
  fs.mkdirSync(path.join(temporary, "screenshots"), { recursive: true, mode: 0o700 });
  try {
    for (const cell of manifest.cells) {
      for (const [kind, descriptor] of Object.entries(cell?.screenshots || {})) {
        if (!descriptor) continue;
        if (typeof descriptor.path !== "string" || !descriptor.path) {
          throw new Error("custom-path journey screenshot path is invalid");
        }
        const requested = path.resolve(sourceRoot, descriptor.path);
        const entry = fs.lstatSync(requested);
        const screenshot = fs.realpathSync.native(requested);
        if (
          !entry.isFile() || entry.isSymbolicLink() ||
          !pathIsWithin(screenshot, sourceRoot)
        ) {
          throw new Error("custom-path journey screenshot escaped its artifact directory");
        }
        const bytes = fs.readFileSync(screenshot);
        if (bytes.length !== descriptor.size || sha256(bytes) !== descriptor.sha256) {
          throw new Error("custom-path journey screenshot changed before governed publication");
        }
        const filename = `${sha256(`${cell.cellId}\0${kind}\0${descriptor.sha256}`)}.png`;
        const copied = path.join(temporary, "screenshots", filename);
        fs.writeFileSync(copied, bytes, { mode: 0o600, flag: "wx" });
        descriptor.path = `screenshots/${filename}`;
      }
    }
    const payload = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`, "utf8");
    fs.writeFileSync(path.join(temporary, "journey-evidence.json"), payload, {
      mode: 0o600,
      flag: "wx",
    });
    fs.renameSync(temporary, destination);
    return {
      bundleKey,
      manifestSha256: sha256(payload),
      cellCount: manifest.cells.length,
    };
  } catch (error) {
    fs.rmSync(temporary, { recursive: true, force: true });
    throw error;
  }
}

function summarizeCoverage(pages, config, planCells, review, selection = null, requiredCoverage = null) {
  const checkedPages = pages.filter((page) => page.outcome === "checked");
  const failures = [];
  const tolerated = [];
  for (const page of pages) {
    if (page.outcome === "checked") continue;
    const explicitReason = typeof page.target.allowFailure === "string" ? page.target.allowFailure.trim() : "";
    const discoveredAllowed = page.target.source === "coordinator" && config.allowDiscoveredTargetFailures;
    const mandatoryContractFailure = ["journey_contract_error", "evidence_error"].includes(page.outcome);
    const row = {
      cellId: page.cellId,
      url: page.target.url,
      targetName: page.target.name || page.target.url,
      stateName: page.target.stateName || "base",
      requestedPath: page.requestedPath,
      finalPath: page.finalPath,
      viewport: page.viewport.name,
      viewportWidth: page.viewport.width,
      viewportHeight: page.viewport.height,
      outcome: page.outcome,
      reason: page.skipReason,
    };
    if (!mandatoryContractFailure && (explicitReason || discoveredAllowed)) {
      tolerated.push({
        ...row,
        allowance: explicitReason || "coordinator-discovered target failure explicitly tolerated",
      });
    } else {
      failures.push(row);
    }
  }
  const requiredCheckedPages = config.development.enabled
    ? Math.min(config.minCheckedPages, planCells.length)
    : config.minCheckedPages;
  const minimumFailure = checkedPages.length < requiredCheckedPages
    ? `checked ${checkedPages.length} page(s), below required minimum ${requiredCheckedPages}`
    : null;
  return {
    failed: failures.length > 0 || Boolean(minimumFailure) || Boolean(review?.coverageFailures?.length) ||
      Boolean(requiredCoverage?.failed),
    checkedPages: checkedPages.length,
    plannedPages: planCells.length,
    requiredCheckedPages,
    readinessEligible: selection?.readinessEligible ?? true,
    coverageMode: selection?.mode || "complete",
    fullDeclaredPages: selection?.fullPlanCount ?? planCells.length,
    pageBudget: config.maxPageCount,
    widthCoverageMode: "sampled-only",
    widthCoverageNote: "Only the listed viewport widths were checked; widths between samples were not inspected.",
    widthCoverage: sampledWidthCoverage(planCells),
    cells: pages.map((page) => ({
      cellId: page.cellId,
      targetName: page.target.name || page.target.url,
      requestedPath: page.requestedPath,
      finalPath: page.finalPath,
      stateName: page.target.stateName || "base",
      viewport: page.viewport,
      outcome: page.outcome,
      checked: page.outcome === "checked",
      sourceBindingStatus: page.sourceBinding?.status || "unbound",
      startedAt: page.startedAt,
      endedAt: page.endedAt,
      durationMs: page.durationMs,
      execution: page.execution || null,
      cacheHit: Boolean(page.cache?.hit),
      cleanupStatus: page.cleanup?.status || "unknown",
    })),
    failures,
    tolerated,
    minimumFailure,
    reviewFailures: review?.coverageFailures || [],
    requiredCoverage,
  };
}

function markdownReport(report) {
  const findings = report.findings;
  const criticalCount = findings.filter((item) => item.severity === "critical").length;
  const warningCount = findings.filter((item) => item.severity === "warning").length;
  const lines = [];
  lines.push("# Formal Web UI Verification Report", "");
  lines.push(`- Report schema: ${report.schemaVersion}`);
  lines.push(`- Run ID: ${report.runId}`);
  lines.push(`- Started: ${report.startedAt}`);
  lines.push(`- Ended: ${report.endedAt}`);
  lines.push(`- Duration: ${report.durationMs} ms`);
  lines.push(`- Browser: ${report.browser}`);
  lines.push(`- Targets: ${report.targets.length}`);
  lines.push(`- Selected page cells: ${report.plan.plannedPageCount}/${report.plan.fullDeclaredPageCount} declared (${report.plan.pageBudget} budget)`);
  lines.push(`- Execution mode: ${report.plan.selection.mode}`);
  lines.push(`- Readiness eligible: ${report.coverage.readinessEligible ? "yes" : "no — development acceleration only"}`);
  lines.push(`- Maximum concurrency: ${report.execution.maxConcurrency}`);
  lines.push(`- Cache hits: ${report.pages.filter((page) => page.cache?.hit).length}`);
  lines.push(`- Pages checked: ${report.pages.filter((page) => page.outcome === "checked").length}`);
  lines.push(`- Pages skipped: ${report.pages.filter((page) => page.skipped).length}`);
  lines.push(`- Coverage gate: ${report.coverage.failed ? "failed" : "passed"}`);
  lines.push(`- Critical findings: ${criticalCount}`);
  lines.push(`- Warning findings: ${warningCount}`, "");
  lines.push(`- Visual review pending: ${report.review.pendingCount}`);
  lines.push(`- Carried visual-review gaps/blocks: ${report.review.carriedGapCount + report.review.carriedBlockedCount}`);
  lines.push(`- Review queue: ${report.review.queuePath}`, "");
  lines.push("## Evidence Identity", "");
  lines.push(`- Verifier SHA-256: ${report.evidence.verifier.sha256}`);
  lines.push(`- Config SHA-256: ${report.evidence.config.sha256}`);
  lines.push(`- Config hash scope: ${report.evidence.config.scope}`);
  lines.push(`- Width coverage: sampled-only — ${report.plan.widthCoverageNote}`, "");
  lines.push("## Pages", "");
  lines.push("| Cell | Plan/exec | Priority | Target | Requested path | Final path | State | Viewport | HTTP | Source binding | Cache | Cleanup | Duration | Result | Findings | Initial viewport | Full page |");
  lines.push("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
  for (const page of report.pages) {
    const result = page.skipped ? `${page.outcome}: ${page.skipReason}` : "checked";
    lines.push(`| ${escapeMd(page.cellId)} | ${(page.execution?.planIndex ?? 0) + 1}/${page.execution?.executionIndex ?? "-"} | ${page.execution?.priority ?? ""} | ${escapeMd(page.target.name || page.target.url)} | ${escapeMd(page.requestedPath || "")} | ${escapeMd(page.finalPath || "")} | ${escapeMd(page.target.stateName || "base")} | ${escapeMd(page.viewport.name)} ${page.viewport.width}x${page.viewport.height} | ${page.status ?? ""} | ${escapeMd(page.sourceBinding?.status || "unbound")} | ${page.cache?.hit ? "hit" : "miss"} | ${escapeMd(page.cleanup?.status || "unknown")} | ${page.durationMs ?? 0} ms | ${escapeMd(result)} | ${(page.findings || []).length} | ${page.screenshots?.viewport ? escapeMd(page.screenshots.viewport.path) : ""} | ${page.screenshots?.fullPage ? escapeMd(page.screenshots.fullPage.path) : ""} |`);
  }
  lines.push("", "## Rendered Performance", "");
  lines.push("Main-document navigation metrics are captured before the verifier's full-page scroll. LCP is document navigation LCP, not post-interaction latency.", "");
  lines.push("| Cell | Target | Viewport | Local server | TTFB | TTFB threshold | TTFB status | LCP | LCP threshold | LCP status |");
  lines.push("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
  for (const page of report.pages) {
    const performanceMetrics = page.metrics?.performance;
    const ttfb = performanceMetrics?.ttfb;
    const lcp = performanceMetrics?.lcp;
    lines.push(`| ${escapeMd(page.cellId)} | ${escapeMd(page.target.name || page.target.url)} | ${escapeMd(page.viewport.name)} | ${performanceMetrics ? (performanceMetrics.localServer ? "yes" : "no") : ""} | ${ttfb?.valueMs ?? "unavailable"} | ${ttfb ? `< ${ttfb.thresholdMs} ms` : ""} | ${escapeMd(ttfb?.status || "not measured")} | ${lcp?.valueMs ?? "unavailable"} | ${lcp ? `< ${lcp.thresholdMs} ms` : ""} | ${escapeMd(lcp?.status || "not measured")} |`);
  }
  lines.push("", "## Execution & Timing", "");
  if (report.authentication.length) {
    lines.push("| Authentication profile | Status | Duration | Error |");
    lines.push("| --- | --- | --- | --- |");
    for (const profile of report.authentication) {
      lines.push(`| ${escapeMd(profile.name)} | ${escapeMd(profile.status)} | ${profile.durationMs} ms | ${escapeMd(profile.error || "")} |`);
    }
    lines.push("");
  }
  lines.push("| Cell | Wait evidence | Actions | Handoffs | Stage timing |");
  lines.push("| --- | --- | --- | --- | --- |");
  for (const page of report.pages) {
    const waits = (page.waitEvidence || []).map((item) => `${item.kind}:${item.durationMs}ms`).join(", ");
    const actions = (page.actionTimings || []).map((item) => `${item.action}:${item.outcome}:${item.durationMs}ms`).join(", ");
    const handoffs = (page.handoffs || []).map((item) => `${item.ownerJourney}/${item.ownerState}`).join(", ");
    const stages = (page.timings?.stages || []).map((item) => `${item.name}:${item.outcome}:${item.durationMs}ms`).join(", ");
    lines.push(`| ${escapeMd(page.cellId)} | ${escapeMd(waits)} | ${escapeMd(actions)} | ${escapeMd(handoffs)} | ${escapeMd(stages)} |`);
  }
  lines.push("", "## Visible Scrollbars", "");
  const scrollbarRows = [];
  for (const page of report.pages) {
    for (const scrollbar of page.metrics?.visibleScrollbars || []) {
      scrollbarRows.push({ page, scrollbar });
    }
  }
  if (!scrollbarRows.length) {
    lines.push("No visible/active scrollbars detected.");
  } else {
    lines.push("| Target | Viewport | Axis | Same-axis depth | Scroll chain | Selector | Rect | Scroll Metrics |");
    lines.push("| --- | --- | --- | --- | --- | --- | --- | --- |");
    for (const row of scrollbarRows) {
      const sb = row.scrollbar;
      const rect = sb.rect ? `${Math.round(sb.rect.width)}x${Math.round(sb.rect.height)} at ${Math.round(sb.rect.x)},${Math.round(sb.rect.y)}` : "";
      const metrics = `scroll ${sb.scrollWidth}x${sb.scrollHeight}; client ${sb.clientWidth}x${sb.clientHeight}; overflow ${sb.overflowX}/${sb.overflowY}`;
      const chain = Array.isArray(sb.scrollChain) ? sb.scrollChain.join(" -> ") : "";
      lines.push(`| ${escapeMd(row.page.target.name || row.page.target.url)} | ${escapeMd(row.page.viewport.name)} | ${escapeMd(sb.axis)} | ${sb.sameAxisDepth ?? ""} | ${escapeMd(chain)} | ${escapeMd(sb.selector)} | ${escapeMd(rect)} | ${escapeMd(metrics)} |`);
    }
  }
  lines.push("", "## Coverage & Unmeasurable", "");
  const coverageRows = [];
  for (const page of report.pages) {
    const notInspected = page.metrics?.notInspected;
    const scroll = page.metrics?.scroll;
    const unmeasurable = page.metrics?.unmeasurableContrast || [];
    const ellipsis = page.metrics?.ellipsisTruncations || [];
    const hidden = page.metrics?.hiddenTextLike || {};
    const pending = page.metrics?.pendingMedia || 0;
    if (page.skipped && !notInspected && !scroll) continue;
    const label = `${page.target.name || page.target.url} (${page.viewport.name})`;
    const shadow = notInspected ? notInspected.openShadowRoots : 0;
    const iframes = notInspected ? notInspected.iframes : 0;
    const inspectedShadow = notInspected ? notInspected.inspectedOpenShadowRoots || 0 : 0;
    const discoveredShadow = notInspected ? notInspected.discoveredOpenShadowRoots || inspectedShadow : 0;
    const inspectedIframes = notInspected ? notInspected.inspectedIframes || 0 : 0;
    const discoveredIframes = notInspected ? notInspected.discoveredIframes || inspectedIframes + iframes : 0;
    const hiddenTotal = (hidden.displayNone || 0) + (hidden.visibilityHidden || 0) + (hidden.zeroOpacity || 0) + (hidden.zeroSize || 0);
    const scrollNote = scroll && !scroll.skipped
      ? `scrolled to ${scroll.scrolledTo}px over ${scroll.scrollPasses} pass(es)${scroll.capped ? " (capped)" : ""}`
      : "scroll off";
    coverageRows.push({ label, shadow, iframes, inspectedShadow, discoveredShadow, inspectedIframes, discoveredIframes, unmeasurable: unmeasurable.length, ellipsis: ellipsis.length, hiddenTotal, pending, scrollNote });
  }
  if (!coverageRows.length) {
    lines.push("No coverage gaps recorded.");
  } else {
    lines.push("| Target | Open shadow roots (inspected/discovered; missed) | Iframes (inspected/discovered; missed) | Unmeasurable contrast | Allowed ellipsis/clamp | Hidden text/controls | Pending media | Scroll pass |");
    lines.push("| --- | --- | --- | --- | --- | --- | --- | --- |");
    for (const row of coverageRows) {
      lines.push(`| ${escapeMd(row.label)} | ${row.inspectedShadow}/${row.discoveredShadow}; ${row.shadow} | ${row.inspectedIframes}/${row.discoveredIframes}; ${row.iframes} | ${row.unmeasurable} | ${row.ellipsis} | ${row.hiddenTotal} | ${row.pending} | ${escapeMd(row.scrollNote)} |`);
    }
  }
  lines.push("", "## Target Coverage", "");
  if (!report.coverage.failures.length && !report.coverage.minimumFailure &&
      !report.coverage.reviewFailures.length && !report.coverage.requiredCoverage?.failed) {
    lines.push(`Coverage passed with ${report.coverage.checkedPages} checked page(s).`);
  } else {
    if (report.coverage.minimumFailure) lines.push(`- ${report.coverage.minimumFailure}`);
    for (const failure of report.coverage.failures) {
      lines.push(`- ${failure.cellId} ${failure.requestedPath} → ${failure.finalPath || "unavailable"} [${failure.stateName}; ${failure.viewport} ${failure.viewportWidth}x${failure.viewportHeight}]: ${failure.outcome} — ${failure.reason}`);
    }
  }
  for (const item of report.coverage.tolerated) {
    lines.push(`- Tolerated ${item.cellId} ${item.requestedPath} [${item.stateName}; ${item.viewport}]: ${item.outcome} — ${item.allowance}`);
  }
  for (const item of report.coverage.reviewFailures || []) {
    lines.push(`- Review coverage failure ${item.reviewCellKey}: ${item.reason}`);
  }
  const requiredCoverage = report.coverage.requiredCoverage;
  if (requiredCoverage?.declaredCount) {
    lines.push(`- Required cells satisfied: ${requiredCoverage.satisfiedCount}/${requiredCoverage.declaredCount}.`);
    for (const item of requiredCoverage.entries.filter((entry) => entry.status !== "satisfied")) {
      lines.push(`- Required coverage ${item.requirementId} ${item.status}: ${item.reason}`);
    }
  }
  lines.push("", "## Changed Visual Review", "");
  lines.push(`- Pending changed/new cells: ${report.review.pendingCount}`);
  lines.push(`- Carried passes: ${report.review.carriedPassCount}`);
  lines.push(`- Carried gaps: ${report.review.carriedGapCount}`);
  lines.push(`- Carried blocks: ${report.review.carriedBlockedCount}`);
  lines.push(`- Queue SHA-256: ${report.review.queueSha256}`);
  lines.push("- Screenshot hashes bind evidence integrity and never trigger review.");
  if (report.review.pendingCount) {
    lines.push("- Agent review is required after automated checks for the queue entries only.");
  } else {
    lines.push("- No changed UI inputs, intent, or new cells require image review in this run.");
  }
  lines.push("", "## Findings", "");
  if (!findings.length) {
    lines.push("No findings.");
  } else {
    lines.push("| Severity | Rule | Cell | Target | State | Viewport | Selector | Evidence |");
    lines.push("| --- | --- | --- | --- | --- | --- | --- | --- |");
    for (const finding of findings) {
      const evidence = finding.textSnippet || JSON.stringify(finding.evidence || {});
      lines.push(`| ${finding.severity} | ${escapeMd(finding.rule)} | ${escapeMd(finding.cellId)} | ${escapeMd(finding.targetName)} | ${escapeMd(finding.stateName)} | ${escapeMd(finding.viewport)} | ${escapeMd(finding.selector)} | ${escapeMd(String(evidence).slice(0, 180))} |`);
    }
  }
  return `${lines.join("\n")}\n`;
}

function escapeMd(value) {
  return String(value).replace(/\|/g, "\\|").replace(/\n/g, " ");
}

function ensureTargets(config) {
  const all = [...config.targets, ...coordinatorTargets(config)];
  const seen = new Set();
  const unique = [];
  for (const target of all) {
    if (!target.url) continue;
    const signature = JSON.stringify({
      url: target.url,
      name: target.name || "",
      includeBase: target.includeBase,
      states: target.states || [],
      areas: target.areas || [],
      contentInsets: target.contentInsets || [],
      ignore: target.ignore || [],
      allowTruncation: target.allowTruncation || [],
      allowOverlap: target.allowOverlap || [],
      allowContrast: target.allowContrast || [],
      themeExceptions: target.themeExceptions || [],
      screenshotMasks: target.screenshotMasks || [],
      journeys: target.journeys || [],
      primaryJourney: target.primaryJourney || null,
      priorityOverrideReason: target.priorityOverrideReason || "",
      regions: target.regions || [],
      theme: target.theme || null,
      reviewInputs: target.reviewInputs || [],
      breakpointProfile: target.breakpointProfile || null,
      sourceBinding: target.sourceBinding || null,
      waitFor: target.waitFor || null,
      performance: target.performance || null,
      allowFailure: target.allowFailure || "",
    });
    if (seen.has(signature)) continue;
    seen.add(signature);
    unique.push({
      ...target,
      targetGroupId: target.targetGroupId || `discovered-target-${unique.length + 1}`,
      execution: normalizeExecution(target.execution, "target.execution", config.targetDefaults.execution),
      authProfile: target.authProfile ?? config.targetDefaults.authProfile,
      performance: {
        ...(config.targetDefaults.performance || {}),
        ...(target.performance || {}),
      },
    });
  }
  if (!unique.length) throw new Error("No targets to verify. Provide --url, --config targets, or --from-coordinator.");
  return unique;
}

function writeReportArtifacts(report, markdown, artifacts) {
  fs.mkdirSync(path.dirname(artifacts.jsonOut), { recursive: true });
  fs.mkdirSync(path.dirname(artifacts.markdownOut), { recursive: true });
  fs.writeFileSync(artifacts.jsonOut, `${JSON.stringify(report, null, 2)}\n`, "utf8");
  fs.writeFileSync(artifacts.markdownOut, markdown, "utf8");
}

function artifactReceipt(artifacts) {
  if (!artifacts) return undefined;
  const jsonDirectory = path.dirname(artifacts.jsonOut);
  const markdownDirectory = path.dirname(artifacts.markdownOut);
  if (jsonDirectory === markdownDirectory) {
    const receipt = {
      directory: jsonDirectory,
      json: path.basename(artifacts.jsonOut),
      markdown: path.basename(artifacts.markdownOut),
    };
    if (artifacts.reviewQueueOut && fs.existsSync(artifacts.reviewQueueOut)) {
      receipt.reviewQueue = path.relative(jsonDirectory, artifacts.reviewQueueOut) || path.basename(artifacts.reviewQueueOut);
    }
    if (artifacts.journeyEvidenceOut && fs.existsSync(artifacts.journeyEvidenceOut)) {
      receipt.journeyEvidence = path.relative(jsonDirectory, artifacts.journeyEvidenceOut) || path.basename(artifacts.journeyEvidenceOut);
    }
    if (artifacts.screenshotDir && fs.existsSync(artifacts.screenshotDir)) {
      receipt.screenshots = path.relative(jsonDirectory, artifacts.screenshotDir) || path.basename(artifacts.screenshotDir);
    }
    if (artifacts.progressOut && fs.existsSync(artifacts.progressOut)) {
      receipt.progress = path.relative(jsonDirectory, artifacts.progressOut) || path.basename(artifacts.progressOut);
    }
    return receipt;
  }
  return {
    json: artifacts.jsonOut,
    markdown: artifacts.markdownOut,
    ...(artifacts.reviewQueueOut && fs.existsSync(artifacts.reviewQueueOut)
      ? { reviewQueue: artifacts.reviewQueueOut }
      : {}),
    ...(artifacts.journeyEvidenceOut && fs.existsSync(artifacts.journeyEvidenceOut)
      ? { journeyEvidence: artifacts.journeyEvidenceOut }
      : {}),
    ...(artifacts.screenshotDir && fs.existsSync(artifacts.screenshotDir)
      ? { screenshots: artifacts.screenshotDir }
      : {}),
    ...(artifacts.progressOut && fs.existsSync(artifacts.progressOut)
      ? { progress: artifacts.progressOut }
      : {}),
  };
}

function emitReceipt(receipt) {
  let line = JSON.stringify(receipt);
  if (Buffer.byteLength(line, "utf8") > RECEIPT_MAX_BYTES) {
    const artifacts = receipt.artifacts
      ? {
          json: path.basename(receipt.artifacts.json || "report.json"),
          markdown: path.basename(receipt.artifacts.markdown || "report.md"),
          reviewQueue: path.basename(receipt.artifacts.reviewQueue || "review-queue.json"),
          journeyEvidence: path.basename(receipt.artifacts.journeyEvidence || "journey-evidence.json"),
          screenshots: path.basename(receipt.artifacts.screenshots || "screenshots"),
          progress: path.basename(receipt.artifacts.progress || "progress.jsonl"),
          pathOmittedForBound: true,
        }
      : undefined;
    line = JSON.stringify({
      tool: "formal-web-ui-verification",
      status: receipt.status,
      exitCode: receipt.exitCode,
      artifacts,
      receiptTruncated: true,
    });
  }
  console.log(line);
}

function resultReceipt(report, exitCode, config, blocking) {
  return {
    tool: "formal-web-ui-verification",
    runId: report.runId,
    status: exitCode === 0
      ? (report.coverage.readinessEligible ? "passed" : "development-passed")
      : (exitCode === 1 ? "blocking-findings" : "coverage-failed"),
    exitCode,
    failOn: config.rules.failOn,
    counts: {
      blocking: blocking.length,
      critical: report.findings.filter((finding) => finding.severity === "critical").length,
      warning: report.findings.filter((finding) => finding.severity === "warning").length,
      checkedPages: report.coverage.checkedPages,
      skippedPages: report.pages.filter((page) => page.skipped).length,
      reviewPending: report.review.pendingCount,
      carriedReviewGaps: report.review.carriedGapCount + report.review.carriedBlockedCount,
      cacheHits: report.pages.filter((page) => page.cache?.hit).length,
      executedCells: report.execution.executedCount,
    },
    coverage: report.coverage.failed ? "failed" : "passed",
    readinessEligible: report.coverage.readinessEligible,
    artifacts: artifactReceipt(activeArtifacts),
  };
}

function errorEvidence(error) {
  const message = String(error?.message || error || "Unknown setup failure");
  const stack = String(error?.stack || message);
  return {
    name: String(error?.name || "Error"),
    message: message.slice(0, 8192),
    stack: stack.slice(0, 32768),
  };
}

function setupFailureArtifacts(error, preferred, fallback) {
  const evidence = errorEvidence(error);
  const endedAt = new Date().toISOString();
  const report = {
    schemaVersion: REPORT_SCHEMA_VERSION,
    runId: `formal-web-ui-${Date.now().toString(36)}-setup`,
    generatedAt: endedAt,
    startedAt: runStartedAt,
    endedAt,
    durationMs: Math.max(0, Date.parse(endedAt) - Date.parse(runStartedAt)),
    status: "setup-failure",
    exitCode: 2,
    error: evidence,
    evidence: {
      verifier: { algorithm: "sha256", sha256: verifierSha256 },
      config: activeConfigSha256
        ? {
            algorithm: "sha256",
            sha256: activeConfigSha256,
            scope: "privacy-safe normalized effective config; action, cookie, auth, and readback values redacted",
          }
        : null,
    },
    targets: [],
    pages: [],
    findings: [],
    coverage: {
      failed: true,
      checkedPages: 0,
      failures: [],
      tolerated: [],
      minimumFailure: "Verification did not start because setup or configuration failed.",
    },
  };
  const markdown = [
    "# Formal Web UI Verification Setup Failure",
    "",
    `- Run: ${report.runId}`,
    `- Started: ${report.startedAt}`,
    `- Ended: ${report.endedAt}`,
    `- Verifier SHA-256: ${verifierSha256}`,
    `- Config SHA-256: ${activeConfigSha256 || "unavailable before configuration normalized"}`,
    "- Exit code: 2",
    `- Error: ${evidence.message.replace(/\r?\n/g, " ")}`,
    "",
    "## Diagnostic",
    "",
    "```text",
    evidence.stack.replace(/```/g, "` ` `"),
    "```",
    "",
  ].join("\n");
  const candidates = [preferred, fallback].filter(Boolean);
  const seen = new Set();
  let writeError;
  for (const artifacts of candidates) {
    const key = `${artifacts.jsonOut}\u0000${artifacts.markdownOut}`;
    if (seen.has(key)) continue;
    seen.add(key);
    try {
      writeReportArtifacts(report, markdown, artifacts);
      return { report, artifacts };
    } catch (errorDuringWrite) {
      writeError = errorDuringWrite;
    }
  }
  return { report, artifacts: undefined, writeError };
}

function applyConfiguredCookies(context, cookies, targetUrl) {
  if (!cookies.length) return Promise.resolve();
  return context.addCookies(cookies.map((cookie) => ({
    name: cookie.name,
    value: cookie.value,
    ...(cookie.domain
      ? { domain: cookie.domain, path: cookie.path || "/" }
      : { url: cookie.url || targetUrl }),
  })));
}

async function prepareAuthentication(browser, config) {
  const states = new Map();
  const report = [];
  for (const profile of config.authProfiles) {
    const started = Date.now();
    let context = null;
    try {
      context = await browser.newContext({
        ...(config.ignoreHttpsErrors ? { ignoreHTTPSErrors: true } : {}),
      });
      await applyConfiguredCookies(context, config.cookies, profile.url);
      const page = await context.newPage();
      await page.goto(profile.url, { waitUntil: "domcontentloaded", timeout: 15000 });
      const execution = await applyInteractionState(page, {
        name: `auth:${profile.name}`,
        actions: profile.actions,
        waitFor: profile.waitFor,
        afterFailureWaitFor: {},
        continuation: null,
      });
      if (execution.failure || execution.handoff) {
        throw new Error(execution.failure?.message || "authentication action unexpectedly handed off");
      }
      const storageState = await context.storageState();
      states.set(profile.name, { ok: true, storageState });
      report.push({ name: profile.name, status: "ready", durationMs: Date.now() - started });
    } catch (error) {
      const message = String(error?.message || error).slice(0, 512);
      states.set(profile.name, { ok: false, error: message });
      report.push({ name: profile.name, status: "failed", durationMs: Date.now() - started, error: message });
    } finally {
      if (context) await context.close().catch(() => {});
    }
  }
  return { states, report };
}

function cacheSecretDigest(config, target) {
  const authProfile = config.authProfiles.find((profile) => profile.name === target.authProfile);
  return sha256(stableJson({
    cookies: config.cookies.map((cookie) => ({ ...cookie })),
    stateActions: (target.verificationState?.actions || []).map((action) => ({
      action: action.action,
      selector: action.selector,
      value: action.value,
    })),
    authActions: (authProfile?.actions || []).map((action) => ({
      action: action.action,
      selector: action.selector,
      value: action.value,
    })),
    readbackExpectations: [
      config.waitFor?.readback?.equals,
      target.waitFor?.readback?.equals,
      target.verificationState?.waitFor?.readback?.equals,
      target.verificationState?.afterFailureWaitFor?.readback?.equals,
      authProfile?.waitFor?.readback?.equals,
    ],
  }));
}

function ensureExplicitCacheRoot(config) {
  const cache = config.development.cache;
  if (!cache) return null;
  if (!fs.existsSync(cache.directory)) {
    throw new Error("development cache directory must already exist");
  }
  if (pathHasSymlinkComponent(cache.directory)) {
    throw new Error("development cache directory must not contain symlinked path components");
  }
  const stat = fs.lstatSync(cache.directory);
  if (!stat.isDirectory() || stat.isSymbolicLink()) {
    throw new Error("development cache directory must be a regular non-symlink directory");
  }
  const real = fs.realpathSync.native(cache.directory);
  if (config.repoRoot && pathIsWithin(real, fs.realpathSync.native(path.resolve(config.repoRoot)))) {
    throw new Error("development cache directory must stay outside repoRoot");
  }
  return real;
}

function cacheKeyForCell(cell, config, browserLabel) {
  const binding = cell.target.sourceBinding || config.sourceBinding;
  if (!binding?.expected) return { key: null, reason: "source-binding-required" };
  if (!cell.target.reviewEvidence?.fingerprint) return { key: null, reason: "review-input-fingerprint-required" };
  const cache = config.development.cache;
  if (!cache) return { key: null, reason: "cache-disabled" };
  return {
    key: sha256(stableJson({
      schema: 1,
      verifierSha256,
      browser: browserLabel,
      configSha256: activeConfigSha256,
      secretDigest: cacheSecretDigest(config, cell.target),
      sourceExpected: binding.expected,
      sourceFingerprint: cell.target.reviewEvidence.fingerprint,
      intentFingerprint: cell.target.intentFingerprint,
      dataRevision: cache.dataRevision,
      target: publicTarget(cell.target),
      viewport: publicViewport(cell.viewport),
    })),
    reason: null,
  };
}

function cacheEntryPath(cacheRoot, key) {
  return path.join(cacheRoot, "v1", key.slice(0, 2), key);
}

function readRegularCacheFile(file, cacheRoot) {
  const resolved = path.resolve(file);
  if (!pathIsWithin(resolved, cacheRoot) || !fs.existsSync(resolved)) {
    throw new Error("cache entry file is missing or outside the cache root");
  }
  if (pathHasSymlinkComponent(resolved)) throw new Error("cache entry contains a symlink");
  const stat = fs.lstatSync(resolved);
  if (!stat.isFile() || stat.isSymbolicLink()) throw new Error("cache entry must contain regular files");
  return fs.readFileSync(resolved);
}

function readCachedCell(cell, config, browserLabel, cacheRoot) {
  const cache = config.development.cache;
  if (!cache || !["read", "read-write"].includes(cache.mode)) return { hit: false, reason: "cache-read-disabled" };
  const identity = cacheKeyForCell(cell, config, browserLabel);
  if (!identity.key) return { hit: false, reason: identity.reason };
  const entry = cacheEntryPath(cacheRoot, identity.key);
  if (!fs.existsSync(entry)) return { hit: false, reason: "not-found", key: identity.key };
  try {
    const manifestBytes = readRegularCacheFile(path.join(entry, "manifest.json"), cacheRoot);
    const manifest = JSON.parse(manifestBytes.toString("utf8"));
    const { integritySha256, ...payload } = manifest;
    if (
      manifest.schemaVersion !== 1 || manifest.key !== identity.key ||
      !SHA256_RE.test(integritySha256 || "") || sha256(stableJson(payload)) !== integritySha256
    ) {
      throw new Error("cache manifest integrity mismatch");
    }
    fs.mkdirSync(config.screenshotDir, { recursive: true, mode: 0o700 });
    const page = structuredClone(manifest.page);
    for (const kind of ["viewport", "fullPage"]) {
      const sourceName = kind === "viewport" ? "viewport.png" : "full-page.png";
      const descriptor = page.screenshots?.[kind];
      if (!descriptor || !SHA256_RE.test(descriptor.sha256 || "")) {
        throw new Error(`cache ${kind} screenshot descriptor is invalid`);
      }
      const bytes = readRegularCacheFile(path.join(entry, sourceName), cacheRoot);
      if (sha256(bytes) !== descriptor.sha256) throw new Error(`cache ${kind} screenshot hash mismatch`);
      pngDimensions(bytes);
      const destination = screenshotArtifactPath(
        config,
        cell.cellId,
        cell.target,
        cell.viewport,
        kind === "viewport" ? "viewport" : "full-page",
      );
      fs.copyFileSync(path.join(entry, sourceName), destination, fs.constants.COPYFILE_EXCL);
      descriptor.path = destination;
    }
    const now = new Date().toISOString();
    page.cellId = cell.cellId;
    page.target = publicTarget(cell.target);
    page.viewport = publicViewport(cell.viewport);
    page.startedAt = now;
    page.endedAt = now;
    page.durationMs = 0;
    page.cachedEvidence = {
      originalStartedAt: manifest.page.startedAt,
      originalEndedAt: manifest.page.endedAt,
      originalDurationMs: manifest.page.durationMs,
      originalTimings: manifest.page.timings,
    };
    page.waitEvidence = [];
    page.actionTimings = [];
    page.timings = { stages: [{ name: "cache-reuse", durationMs: 0, outcome: "completed" }], totalMs: 0 };
    page.screenshot = page.screenshots.viewport.path;
    page.cache = { hit: true, key: identity.key, createdAt: manifest.createdAt };
    return { hit: true, key: identity.key, page };
  } catch (error) {
    return { hit: false, reason: `rejected:${String(error?.message || error).slice(0, 240)}`, key: identity.key };
  }
}

function writeCachedCell(cell, page, config, browserLabel, cacheRoot) {
  const cache = config.development.cache;
  if (!cache || !["write", "read-write"].includes(cache.mode)) return { written: false, reason: "cache-write-disabled" };
  const identity = cacheKeyForCell(cell, config, browserLabel);
  if (!identity.key) return { written: false, reason: identity.reason };
  const failThreshold = SEVERITY_ORDER[config.rules.failOn];
  if (
    page.outcome !== "checked" || page.evidenceErrors?.length ||
    page.sourceBinding?.status !== "matched" ||
    (page.findings || []).some((finding) => SEVERITY_ORDER[finding.severity] >= failThreshold) ||
    !page.screenshots?.viewport?.path || !page.screenshots?.fullPage?.path
  ) {
    return { written: false, reason: "cell-not-successful" };
  }
  const entry = cacheEntryPath(cacheRoot, identity.key);
  if (fs.existsSync(entry)) return { written: false, reason: "already-present", key: identity.key };
  const parent = path.dirname(entry);
  fs.mkdirSync(parent, { recursive: true, mode: 0o700 });
  if (pathHasSymlinkComponent(parent)) throw new Error("cache entry parent contains a symlink");
  const temporary = path.join(parent, `.${identity.key}.tmp-${process.pid}-${Date.now()}`);
  fs.mkdirSync(temporary, { mode: 0o700 });
  try {
    const cachedPage = structuredClone(page);
    delete cachedPage.cache;
    delete cachedPage.execution;
    cachedPage.screenshots.viewport.path = "viewport.png";
    cachedPage.screenshots.fullPage.path = "full-page.png";
    cachedPage.screenshot = "viewport.png";
    fs.copyFileSync(page.screenshots.viewport.path, path.join(temporary, "viewport.png"), fs.constants.COPYFILE_EXCL);
    fs.copyFileSync(page.screenshots.fullPage.path, path.join(temporary, "full-page.png"), fs.constants.COPYFILE_EXCL);
    const payload = {
      schemaVersion: 1,
      key: identity.key,
      createdAt: new Date().toISOString(),
      page: cachedPage,
    };
    const manifest = { ...payload, integritySha256: sha256(stableJson(payload)) };
    fs.writeFileSync(path.join(temporary, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`, {
      encoding: "utf8",
      mode: 0o600,
      flag: "wx",
    });
    fs.renameSync(temporary, entry);
    return { written: true, key: identity.key };
  } catch (error) {
    fs.rmSync(temporary, { recursive: true, force: true });
    if (fs.existsSync(entry)) return { written: false, reason: "concurrent-entry", key: identity.key };
    throw error;
  }
}

function syntheticCellResult(cell, outcome, reason) {
  const now = new Date().toISOString();
  const requested = routeEvidence(cell.target.url);
  return {
    cellId: cell.cellId,
    target: publicTarget(cell.target),
    viewport: publicViewport(cell.viewport),
    skipped: true,
    outcome,
    skipReason: reason,
    url: cell.target.url,
    requestedPath: requested.path,
    finalPath: null,
    requestedOrigin: requested.origin,
    finalOrigin: null,
    redirected: false,
    sourceBinding: { status: "unbound", expected: null, observed: null, observedFrom: null },
    startedAt: now,
    endedAt: now,
    durationMs: 0,
    status: null,
    contentType: null,
    title: "",
    metrics: {},
    findings: [],
    screenshot: null,
    screenshots: { viewport: null, fullPage: null },
    evidenceErrors: [],
    continuation: { checked: false, evidence: null },
    handoffs: [],
    waitEvidence: [],
    actionTimings: [],
    timings: { stages: [], totalMs: 0 },
    review: {
      reviewCellKey: null,
      sourceFingerprint: cell.target.reviewEvidence?.fingerprint || null,
      intentFingerprint: cell.target.intentFingerprint || null,
    },
  };
}

function initializeProgress(config, runId, selection) {
  fs.mkdirSync(path.dirname(config.progressOut), { recursive: true });
  fs.writeFileSync(config.progressOut, `${JSON.stringify({
    kind: "run-start",
    runId,
    startedAt: runStartedAt,
    selection: {
      mode: selection.mode,
      fullPlanCount: selection.fullPlanCount,
      selectedCount: selection.selectedCount,
      readinessEligible: selection.readinessEligible,
    },
  })}\n`, { encoding: "utf8", mode: 0o600 });
}

function appendProgress(config, page) {
  const row = {
    kind: "cell-complete",
    cellId: page.cellId,
    planIndex: page.execution?.planIndex,
    executionIndex: page.execution?.executionIndex,
    targetName: page.target.name || page.requestedPath || "target",
    stateName: page.target.stateName || "base",
    viewport: page.viewport.name,
    outcome: page.outcome,
    durationMs: page.durationMs,
    cacheHit: Boolean(page.cache?.hit),
    cleanup: page.cleanup?.status || "unknown",
  };
  fs.appendFileSync(config.progressOut, `${JSON.stringify(row)}\n`, "utf8");
}

function finalizeProgress(config, report) {
  fs.appendFileSync(config.progressOut, `${JSON.stringify({
    kind: "run-complete",
    runId: report.runId,
    endedAt: report.endedAt,
    durationMs: report.durationMs,
    checkedPages: report.coverage.checkedPages,
    failed: report.coverage.failed,
    readinessEligible: report.coverage.readinessEligible,
    unsafeStop: report.execution.unsafeStop,
  })}\n`, "utf8");
}

async function runVerificationCell(browser, cell, config, browserLabel, authStates, cacheRoot, executionIndex) {
  const execution = {
    planIndex: cell.planIndex,
    executionIndex,
    priority: cell.executionPriority,
    parallelSafe: cell.target.execution.parallelSafe,
    resourceLocks: cell.target.execution.resourceLocks,
    stopOnFailure: cell.target.execution.stopOnFailure,
    stopReason: cell.target.execution.stopReason,
  };
  const profile = cell.target.authProfile ? authStates.get(cell.target.authProfile) : null;
  if (profile && !profile.ok) {
    const page = syntheticCellResult(cell, "auth_setup_error", `authentication profile failed: ${profile.error}`);
    page.execution = execution;
    page.cleanup = { status: "not-required" };
    return {
      page,
      unsafeStop: cell.target.execution.stopOnFailure
        ? `declared-unsafe:${cell.target.execution.stopReason}`
        : null,
    };
  }
  let cacheRead = { hit: false, reason: "cache-disabled" };
  if (cacheRoot) {
    cacheRead = readCachedCell(cell, config, browserLabel, cacheRoot);
    if (cacheRead.hit) {
      cacheRead.page.execution = execution;
      cacheRead.page.cleanup = { status: "not-required" };
      return { page: cacheRead.page, unsafeStop: null };
    }
  }
  let context = null;
  let pageResult = null;
  let cleanupError = null;
  try {
    context = await browser.newContext({
      ...cell.viewport.contextOptions,
      ...(profile?.storageState ? { storageState: profile.storageState } : {}),
      ...(config.ignoreHttpsErrors ? { ignoreHTTPSErrors: true } : {}),
    });
    await applyConfiguredCookies(context, config.cookies, cell.target.url);
    const page = await context.newPage();
    pageResult = await verifyTarget(page, cell.target, cell.viewport, config, cell.cellId);
  } catch (error) {
    pageResult = syntheticCellResult(
      cell,
      "internal_cell_error",
      `cell execution failed: ${String(error?.message || error).slice(0, 512)}`,
    );
  } finally {
    if (context) {
      try {
        await context.close();
      } catch (error) {
        cleanupError = String(error?.message || error).slice(0, 512);
      }
    }
  }
  pageResult.execution = execution;
  pageResult.cleanup = cleanupError
    ? { status: "failed", error: cleanupError }
    : { status: "completed" };
  pageResult.cache = { hit: false, reason: cacheRead.reason, key: cacheRead.key || null };
  if (cleanupError && pageResult.outcome === "checked") {
    pageResult.skipped = true;
    pageResult.outcome = "cleanup_error";
    pageResult.skipReason = `isolated browser context cleanup failed: ${cleanupError}`;
  }
  if (cacheRoot) {
    try {
      pageResult.cache.write = writeCachedCell(cell, pageResult, config, browserLabel, cacheRoot);
    } catch (error) {
      pageResult.cache.write = { written: false, reason: `error:${String(error?.message || error).slice(0, 240)}` };
    }
  }
  const declaredUnsafe = pageResult.outcome !== "checked" && cell.target.execution.stopOnFailure
    ? `declared-unsafe:${cell.target.execution.stopReason}`
    : null;
  return {
    page: pageResult,
    unsafeStop: browser.isConnected() ? declaredUnsafe : "browser-authority-lost",
  };
}

function canRunCell(cell, running) {
  const execution = cell.target.execution;
  if (!execution.parallelSafe) return running.length === 0;
  if (running.some((entry) => !entry.cell.target.execution.parallelSafe)) return false;
  const activeLocks = new Set(running.flatMap((entry) => entry.cell.target.execution.resourceLocks));
  return !execution.resourceLocks.some((lock) => activeLocks.has(lock));
}

async function executePlan(
  browser,
  cells,
  config,
  browserLabel,
  authStates,
  cacheRoot,
  cellRunner = runVerificationCell,
) {
  const pending = [...cells].sort((left, right) =>
    right.executionPriority - left.executionPriority || left.planIndex - right.planIndex
  );
  const running = [];
  const results = new Map();
  let executionCounter = 0;
  let unsafeStop = null;
  const startCell = (cell) => {
    executionCounter += 1;
    const entry = { cell, promise: null };
    entry.promise = cellRunner(
      browser,
      cell,
      config,
      browserLabel,
      authStates,
      cacheRoot,
      executionCounter,
    ).then((value) => ({ entry, value }));
    running.push(entry);
  };
  while ((pending.length || running.length) && !unsafeStop) {
    let launched = false;
    for (let index = 0; index < pending.length && running.length < config.execution.maxConcurrency;) {
      const cell = pending[index];
      if (!canRunCell(cell, running)) {
        index += 1;
        continue;
      }
      pending.splice(index, 1);
      startCell(cell);
      launched = true;
    }
    if (!running.length && pending.length) {
      startCell(pending.shift());
      launched = true;
    }
    if (!running.length) break;
    if (!launched || running.length >= config.execution.maxConcurrency || !pending.length) {
      const completed = await Promise.race(running.map((entry) => entry.promise));
      running.splice(running.indexOf(completed.entry), 1);
      results.set(completed.entry.cell.planIndex, completed.value.page);
      appendProgress(config, completed.value.page);
      if (completed.value.unsafeStop) unsafeStop = completed.value.unsafeStop;
    }
  }
  while (running.length) {
    const completed = await Promise.race(running.map((entry) => entry.promise));
    running.splice(running.indexOf(completed.entry), 1);
    results.set(completed.entry.cell.planIndex, completed.value.page);
    appendProgress(config, completed.value.page);
    if (completed.value.unsafeStop && !unsafeStop) unsafeStop = completed.value.unsafeStop;
  }
  if (unsafeStop) {
    for (const cell of pending) {
      const page = syntheticCellResult(cell, "unsafe_stop_unexecuted", `not executed after ${unsafeStop}`);
      page.execution = {
        planIndex: cell.planIndex,
        executionIndex: null,
        priority: cell.executionPriority,
        parallelSafe: cell.target.execution.parallelSafe,
        resourceLocks: cell.target.execution.resourceLocks,
        stopOnFailure: cell.target.execution.stopOnFailure,
        stopReason: cell.target.execution.stopReason,
      };
      page.cleanup = { status: "not-required" };
      results.set(cell.planIndex, page);
      appendProgress(config, page);
    }
  }
  return {
    pages: [...results.entries()].sort(([left], [right]) => left - right).map(([, page]) => page),
    unsafeStop,
    executionCount: executionCounter,
  };
}

async function main() {
  const rawArgs = process.argv.slice(2);
  if (rawArgs.includes("--help") || rawArgs.includes("-h")) {
    console.log(usage());
    return;
  }
  fallbackArtifacts = createDefaultArtifacts();
  activeArtifacts = fallbackArtifacts;
  const cli = parseArgs(rawArgs);
  activeArtifacts = resolveArtifactPaths({}, cli, fallbackArtifacts);
  const rawConfig = loadConfig(cli.configPath);
  activeArtifacts = resolveArtifactPaths(rawConfig, cli, fallbackArtifacts);
  const config = normalizeConfig(rawConfig, cli, activeArtifacts);
  activeConfigSha256 = sha256(stableJson(privacySafeConfigContract(config)));
  const targets = prepareTargetContracts(expandTargetStates(ensureTargets(config)), config);
  const { chromium, devices } = resolvePlaywright(config.playwrightModuleDir);
  config.viewports = resolveViewports(config.viewports, devices);
  const fullPlanCells = buildExecutionPlan(targets, config.viewports, config.maxPageCount);
  const requiredCoverage = evaluateRequiredCoverage(fullPlanCells, config.requiredCoverage);
  const selected = selectExecutionCells(fullPlanCells, config.development);
  const planCells = selected.cells;
  if (config.development.cache) {
    const cacheIneligible = planCells.find((cell) =>
      !((cell.target.sourceBinding || config.sourceBinding)?.expected) ||
      !cell.target.reviewEvidence?.fingerprint
    );
    if (cacheIneligible) {
      throw new Error(
        `development cache requires sourceBinding.expected and valid reviewInputs for every selected cell (${cacheIneligible.cellId})`,
      );
    }
  }
  const cacheRoot = ensureExplicitCacheRoot(config);
  const { browser, browserLabel } = await launchBrowser(chromium, config.browserExecutable);
  const authentication = await prepareAuthentication(browser, config);
  const report = {
    schemaVersion: REPORT_SCHEMA_VERSION,
    runId: `formal-web-ui-${Date.now().toString(36)}-${randomBytes(4).toString("hex")}`,
    generatedAt: null,
    startedAt: runStartedAt,
    endedAt: null,
    durationMs: null,
    browser: browserLabel,
    targets: targets.map(publicTarget),
    plan: publicExecutionPlan(planCells, config.maxPageCount, selected.selection, requiredCoverage),
    evidence: {
      verifier: { algorithm: "sha256", sha256: verifierSha256 },
      config: {
        algorithm: "sha256",
        sha256: activeConfigSha256,
        scope: "privacy-safe normalized effective config; action, cookie, auth, and readback values redacted",
      },
    },
    pages: [],
    findings: [],
    review: null,
    authentication: authentication.report,
    execution: {
      maxConcurrency: config.execution.maxConcurrency,
      readinessEligible: selected.selection.readinessEligible,
      progressPath: config.progressOut,
      unsafeStop: null,
      executedCount: 0,
    },
  };
  initializeProgress(config, report.runId, selected.selection);
  try {
    const execution = await executePlan(
      browser,
      planCells,
      config,
      browserLabel,
      authentication.states,
      cacheRoot,
    );
    report.pages = execution.pages;
    report.execution.unsafeStop = execution.unsafeStop;
    report.execution.executedCount = execution.executionCount;
  } finally {
    await browser.close().catch(() => {});
  }
  const changedReview = buildChangedReviewQueue(report.pages, config, report.runId);
  const queueSha256 = writeReviewQueueArtifact(changedReview.queue, config.reviewQueueOut);
  report.review = {
    ...changedReview.report,
    queuePath: config.reviewQueueOut,
    queueSha256,
    trigger: changedReview.queue.trigger,
  };
  report.findings = [...summarizeFindings(report.pages), ...changedReview.blockingFindings];
  report.coverage = summarizeCoverage(
    report.pages,
    config,
    planCells,
    report.review,
    selected.selection,
    requiredCoverage,
  );
  report.endedAt = new Date().toISOString();
  report.generatedAt = report.endedAt;
  report.durationMs = Math.max(0, Date.parse(report.endedAt) - Date.parse(report.startedAt));
  finalizeProgress(config, report);
  report.evidence.journey = writeJourneyEvidenceArtifact(report, config.journeyEvidenceOut);
  publishGovernedJourneyEvidenceArtifact(config.journeyEvidenceOut);
  const markdown = markdownReport(report);
  writeReportArtifacts(report, markdown, activeArtifacts);
  const failThreshold = SEVERITY_ORDER[config.rules.failOn];
  const blocking = report.findings.filter((finding) => SEVERITY_ORDER[finding.severity] >= failThreshold);
  const exitCode = report.coverage.failed ? 3 : (blocking.length ? 1 : 0);
  if (config.humanReadableStdout) {
    console.log(markdown);
  } else {
    emitReceipt(resultReceipt(report, exitCode, config, blocking));
  }
  removeUnusedDefaultArtifacts(fallbackArtifacts, activeArtifacts);
  process.exit(exitCode);
}

export { evaluateRequiredCoverage, executePlan, isLocalServerUrl, normalizeRequiredCoverage, pageVerifier, performanceThresholdStatus, writeJourneyEvidenceArtifact };

let isEntrypoint = false;
if (process.argv[1]) {
  try {
    isEntrypoint = fs.realpathSync.native(process.argv[1]) === fs.realpathSync.native(VERIFIER_PATH);
  } catch {
    isEntrypoint = path.resolve(process.argv[1]) === path.resolve(VERIFIER_PATH);
  }
}
if (isEntrypoint) {
  main().catch((error) => {
    const failure = setupFailureArtifacts(error, activeArtifacts, fallbackArtifacts);
    activeArtifacts = failure.artifacts;
    emitReceipt({
      tool: "formal-web-ui-verification",
      runId: failure.report.runId,
      status: "setup-failure",
      exitCode: 2,
      artifacts: artifactReceipt(failure.artifacts),
      artifactStatus: failure.artifacts ? "written" : "unavailable",
    });
    if (failure.artifacts) removeUnusedDefaultArtifacts(fallbackArtifacts, failure.artifacts);
    process.exit(2);
  });
}
