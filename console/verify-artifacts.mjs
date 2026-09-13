import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import path from 'node:path';

const RUN = 't20260101T000100Z-caf123';
const EARLIER_RUN = 't20260101T000000Z-caf122';
const MANIFEST = 'a'.repeat(64);
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk/x8AAusB9Y9Z4rUAAAAASUVORK5CYII=', 'base64');
const FILES = new Map([
  ['capture.png', PNG],
  ['11111111-1111-4111-8111-111111111111-child.png', PNG],
  ['22222222-2222-4222-8222-222222222222-child.png', PNG],
  ['facts.json', Buffer.from('{"component":"R1","style":"Italic","script":"validate.js","reference":"000123","color":"#abcdef","count":0,"enabled":false,"amount":9007199254740993,"ratio":0.0000000000000000001,"comparison":"Expected < 3, actual > 4","records":[{"name":"First","status":"Passed"},{"name":"Second","status":"Failed"}]}')],
  ['broken.xml', Buffer.from('<report><result>Failed')],
  ['external.xml', Buffer.from('<!DOCTYPE report [<!ENTITY payload SYSTEM "https://untrusted.example/secret">]><report>&payload;</report>')],
  ['opaque.json', Buffer.from(JSON.stringify({ sha256: 'c'.repeat(64), uuid: '11111111-1111-4111-8111-111111111111' }))],
  ['0305cfd9-5196-4424-a426-22aed07a6cb0-item-delta-desired.xml', Buffer.from('<schematic-data version="1" xmlns="urn:example:schematic"><kicad.schematic.types.SchematicScreenData><metadata><document><project><name>Controller</name><path>/home/example/.devcoordinator/test/scratch/fixture</path></project></document><title_block><title><value>Power supply</value></title><revision><value>A.1</value></revision><company><value>Fixture engineering</value></company></title_block><page><width_mm>200</width_mm><offset_nm>123456789</offset_nm><distance_nm><value>2500000</value></distance_nm><height_mm>300</height_mm><enabled>false</enabled><uuid>0305cfd9-5196-4424-a426-22aed07a6cb0</uuid><sha256>' + 'd'.repeat(64) + '</sha256></page></metadata></kicad.schematic.types.SchematicScreenData></schematic-data>')],
  ['results.trx', Buffer.from('<TestRun id="0305cfd9-5196-4424-a426-22aed07a6cb0" name="Editor checks"><ResultSummary outcome="Passed"><Counters total="2" passed="2" failed="0" /></ResultSummary><Results><UnitTestResult testId="0305cfd9-5196-4424-a426-22aed07a6cb0" testName="Insert symbol" outcome="Passed" duration="00:00:00.023" /></Results></TestRun>')],
  ['report.json', Buffer.from(JSON.stringify({ result: 'passed', hostile: '<img src=x onerror="window.artifactInjection=true">' }))],
  ['large.txt', Buffer.from('retained output\n'.repeat(80000))],
  ['unpreviewed.bin', Buffer.from([0, 1, 2, 255])],
  ...Array.from({ length: 99 }, (_, index) => [`nested/check-${index}.txt`, Buffer.from(`check ${index}\n`)]),
]);
const entries = [...FILES].map(([file, buffer]) => ({ path: file, size: buffer.length, sha256: crypto.createHash('sha256').update(buffer).digest('hex') }));
const artifact = { name: 'native-evidence', files: entries.length, size: entries.reduce((total, entry) => total + entry.size, 0), sha256: 'b'.repeat(64) };

export function artifactResponse(command, params, scenario) {
  if (!scenario.artifactFiles) return null;
  if (command === 'test.list') return { ok: true, data: { runs: [{
    run_id: RUN, test: 'native-editor', status: 'passed', display_name: 'Native editor',
    worktree_path: '/srv/repos/native-editor', worktree_id: 'native-worktree',
    repository_id: 'native-repository', started_at: '2026-01-01T00:01:00Z', duration_seconds: 4,
    checks_truncated: !!scenario.artifactSummaryTruncated,
    checks: scenario.artifactCurrentEmpty || scenario.artifactSummaryTruncated ? [] : [{ name: 'native-session', status: 'passed', retained_artifacts: [artifact] }],
    visual_evidence: { status: 'unavailable', bundle_count: 0, image_count: 0, issue_count: 0 },
  }] } };
  if (command === 'test.history') {
    assert.equal(params.path, '/srv/repos/native-editor');
    assert.ok(params.limit <= 50);
    if (params.before) assert.equal(params.before, RUN);
    return { ok: true, data: {
      runs: [{ run_id: params.before ? EARLIER_RUN : RUN, test: params.before ? 'earlier-native-editor' : 'native-editor',
        status: 'passed', started_at: params.before ? '2026-01-01T00:00:00Z' : '2026-01-01T00:01:00Z', finished_at: null, duration_seconds: 4 }],
      next_before: params.before ? null : RUN,
    } };
  }
  if (command === 'test.log.catalog') {
    assert.equal(params.phase, undefined);
    assert.ok([RUN, EARLIER_RUN].includes(params.run_id));
    return { ok: true, data: {
      entries: [{ log_ref: { run_id: params.run_id, check: params.run_id === EARLIER_RUN ? 'earlier-session' : 'native-session', phase: 'check', stream: 'stdout' } }],
      next_cursor: null,
    } };
  }
  if (!command.startsWith('test.artifact.')) return null;
  if (scenario.artifactDenied) return { ok: false, error: { code: 'permission_denied', message: 'Administrator access required.', detail: '' } };
  if (scenario.artifactExpired) return { ok: false, error: { code: 'test_artifact_expired', message: 'Retained files have expired.', detail: '' } };
  assert.equal(params.path, '/srv/repos/native-editor');
  assert.ok([RUN, EARLIER_RUN].includes(params.run_id));
  assert.equal(params.check, params.run_id === EARLIER_RUN ? 'earlier-session' : 'native-session');
  if (params.artifact) assert.equal(params.artifact, artifact.name);
  if (command === 'test.artifact.catalog') {
    if (params.offset) assert.equal(params.manifest_sha256, MANIFEST);
    const offset = params.offset || 0;
    const end = Math.min(entries.length, offset + params.limit);
    return { ok: true, data: { run_id: params.run_id, check: params.check, manifest_sha256: MANIFEST, artifact, artifacts: [artifact], entries: entries.slice(offset, end), next_offset: end < entries.length ? end : null } };
  }
  assert.equal(command, 'test.artifact.file');
  assert.equal(params.manifest_sha256, MANIFEST);
  assert.ok(params.max_bytes > 0 && params.max_bytes <= 184320);
  const entry = entries.find((item) => item.path === params.file);
  const source = FILES.get(params.file);
  assert.ok(entry && source);
  const block = source.subarray(params.offset, params.offset + params.max_bytes);
  const next = params.offset + block.length;
  return { ok: true, data: { run_id: params.run_id, check: params.check, artifact: artifact.name, file: params.file,
    sha256: scenario.artifactChanged ? 'c'.repeat(64) : entry.sha256,
    total_bytes: entry.size, offset: params.offset, bytes: block.length, base64: block.toString('base64'), next_offset: next < entry.size ? next : null } };
}

export async function verifyTestArtifacts({ page, daemon, check, baseUrl, output, theme, viewport }) {
  const prefix = `files ${theme} ${viewport.width}`;
  const verify = (name, value, detail = '') => check(`${prefix}: ${name}`, value, detail);
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  daemon.setScenario({ artifactFiles: true });
  await page.goto(`${baseUrl}#/tests?repository=native-repository`);
  await page.waitForSelector('.test-result-summary');
  await page.locator('.test-result-summary').click();
  verify('retained files are discoverable without a formal web bundle', await page.locator('[data-test-artifacts]').count() === 1);
  if (!await page.locator('[data-test-artifacts]').count()) return;
  verify('absence of a formal bundle is not absence of evidence', !/Evidence not produced|Evidence pending/.test(await page.locator('main').innerText()));
  await page.locator('.test-thumbnail').first().waitFor();
  verify('native screenshots also have compact inline thumbnails', (await page.locator('.test-thumbnail img').first().boundingBox()).width <= 112);
  await page.locator('.test-thumbnail').first().click();
  await page.locator('.test-image-preview[open]').waitFor();
  await page.getByRole('button', { name: 'Open file', exact: true }).click();
  await page.locator('#test-artifacts-dialog img').waitFor();
  verify('native thumbnail opens its exact retained file', await page.locator('[data-artifact-file="capture.png"][aria-current=true]').count() === 1);
  await page.keyboard.press('Escape');
  await page.locator('[data-test-artifacts]').click();
  const dialog = page.locator('#test-artifacts-dialog');
  await dialog.locator('img').waitFor();
  await dialog.locator('img').evaluate((image) => image.decode());
  verify('image preview decodes', await dialog.locator('img').evaluate((image) => image.complete && image.naturalWidth === 1));
  verify('opening keeps continuation in view', await dialog.evaluate((element) => element.contains(document.activeElement) && element.getBoundingClientRect().top >= 0));
  await page.screenshot({ path: path.join(output, `files-${theme}-${viewport.width}.png`) });
  await dialog.locator('[data-artifact-file="0305cfd9-5196-4424-a426-22aed07a6cb0-item-delta-desired.xml"]').click();
  await page.waitForFunction(() => document.querySelector('.artifact-content')?.textContent.includes('Power supply'));
  const readable = await dialog.locator('.artifact-content').innerText();
  verify('XML evidence shows data without markup or generated identifiers', !/<schematic-data|<value>|0305cfd9|dddddddd/.test(readable));
  verify('XML dimensions retain exact values with readable units', /Offset \(mm\)\s+123\.456789/.test(readable) && /Distance \(mm\)\s+2\.5/.test(readable));
  verify('meaningful document details precede technical metadata', /Title block/.test(await dialog.locator('.artifact-data > .artifact-data-group > summary').first().innerText()));
  verify('XML evidence keeps meaningful values', /Power supply/.test(readable) && /Fixture engineering/.test(readable) && /A\.1/.test(readable));
  verify('evidence labels omit generated identifiers', !/0305cfd9/.test(await dialog.locator('.artifact-navigation').innerText()));
  verify('structured data values have syntax highlighting', await dialog.locator('.artifact-content .log-token').count() > 0);
  await dialog.locator('[data-artifact-file="report.json"]').click();
  await dialog.locator('.artifact-fields').first().waitFor();
  verify('reports show meaningful data without active or visible markup', (await dialog.locator('.artifact-content').innerText()).includes('passed') && !(await dialog.locator('.artifact-content').innerText()).includes('<img') && !await page.evaluate(() => !!window.artifactInjection));
  verify('duplicate friendly labels are distinguishable without hashes', await dialog.getByRole('button', { name: 'Child · 1 Screenshot', exact: true }).count() === 1 && await dialog.getByRole('button', { name: 'Child · 2 Screenshot', exact: true }).count() === 1);
  await dialog.locator('[data-artifact-file="facts.json"]').click();
  await dialog.locator('.artifact-fields').first().waitFor();
  const facts = await dialog.locator('.artifact-content').innerText();
  verify('meaningful identifiers and comparison text are preserved', facts.includes('R1') && facts.includes('Italic') && facts.includes('validate.js') && facts.includes('000123') && facts.includes('#abcdef') && facts.includes('Expected < 3, actual > 4'));
  verify('numeric precision, zero and false are preserved', facts.includes('9,007,199,254,740,993') && facts.includes('0.0000000000000000001') && facts.includes('false') && /Count\s+0/.test(facts));
  for (let step = 0; await dialog.locator('.artifact-data-group:not([open]) > summary').count(); step += 1) {
    assert.ok(step < 20);
    await dialog.locator('.artifact-data-group:not([open]) > summary').first().click();
  }
  verify('nested entries open with actual highlighted outcomes', await dialog.locator('.artifact-data .log-token-success').count() > 0 && await dialog.locator('.artifact-data .log-token-failure').count() > 0);
  await dialog.locator('[data-artifact-file="results.trx"]').click();
  await dialog.locator('.artifact-fields').first().waitFor();
  for (let step = 0; await dialog.locator('.artifact-data-group:not([open]) > summary').count(); step += 1) {
    assert.ok(step < 20);
    await dialog.locator('.artifact-data-group:not([open]) > summary').first().click();
  }
  verify('test reports show names and outcomes rather than XML', (await dialog.locator('.artifact-content').innerText()).includes('Insert symbol') && !/<TestRun|testId|0305cfd9/.test(await dialog.locator('.artifact-content').innerText()));
  for (const file of ['broken.xml', 'external.xml']) {
    await dialog.locator('[data-artifact-file="' + file + '"]').click();
    await dialog.locator('.artifact-data-unavailable').waitFor();
    verify(file + ' keeps an honest unavailable preview with original download', !await dialog.locator('.artifact-content pre').count() && await dialog.locator('[data-artifact-download]').isEnabled());
  }
  await dialog.locator('[data-artifact-file="opaque.json"]').click();
  await dialog.getByText('No readable data in this file. The original is available to download.', { exact: true }).waitFor();
  verify('opaque-only documents never invent useful values', !/cccccccc|11111111/.test(await dialog.locator('.artifact-content').innerText()));
  await dialog.locator('[data-artifact-file="report.json"]').click();
  await dialog.locator('.artifact-fields').first().waitFor();
  const downloadEvent = page.waitForEvent('download');
  await dialog.getByRole('button', { name: 'Download file', exact: true }).click();
  const download = await downloadEvent;
  verify('download preserves original bytes', (await fs.readFile(await download.path())).equals(FILES.get('report.json')));
  await dialog.getByRole('button', { name: 'More files', exact: true }).click();
  await dialog.locator('[data-artifact-file="nested/check-98.txt"]').waitFor();
  verify('catalogue follows its bounded continuation', await dialog.locator('[data-artifact-file]').count() === entries.length);
  verify('pagination preserves the selected report', await dialog.locator('.artifact-fields').count() === 1);
  await dialog.locator('[data-artifact-file="large.txt"]').click();
  await dialog.getByText('Preview limited to 1 MiB. Download the file for the complete content.', { exact: true }).waitFor();
  verify('large text is explicitly bounded', (await dialog.locator('pre').innerText()).length <= 1048576);
  await dialog.locator('[data-artifact-file="unpreviewed.bin"]').click();
  await dialog.getByText('No browser preview for this file type.', { exact: true }).waitFor();
  verify('binary is downloadable without rendering active content', await dialog.getByRole('button', { name: 'Download file', exact: true }).isEnabled());
  verify('dialog does not overflow the viewport', await dialog.evaluate((element) => element.getBoundingClientRect().right <= innerWidth && element.getBoundingClientRect().bottom <= innerHeight && element.scrollWidth <= element.clientWidth));
  await page.keyboard.press('Escape');
  verify('cancel restores focus', await page.locator('[data-test-artifacts]').evaluate((element) => element === document.activeElement));
  for (const [scenario, message] of [['artifactExpired', 'Retained files have expired.'], ['artifactDenied', 'Administrator access required.'], ['artifactChanged', 'The retained file changed or its response was incomplete.']]) {
    daemon.setScenario({ artifactFiles: true, [scenario]: true });
    await page.locator('[data-test-artifacts]').click();
    await dialog.getByText(message, { exact: true }).waitFor();
    verify(`${scenario} stays honest and does not show a stale image`, await dialog.locator('img').count() === 0);
    daemon.setScenario({ artifactFiles: true });
    await dialog.getByRole('button', { name: 'Try again', exact: true }).click();
    await dialog.locator('img').waitFor();
    await dialog.locator('img').evaluate((image) => image.decode());
    verify(`${scenario} recovers on exact retry`, await dialog.locator('img').evaluate((image) => image.naturalWidth === 1));
    await dialog.getByRole('button', { name: 'Close evidence files', exact: true }).click();
  }
  daemon.setScenario({ artifactFiles: true, artifactSummaryTruncated: true });
  await page.goto(`${baseUrl}#/tests?repository=native-repository`);
  await page.locator('[data-test-artifacts]').click();
  await page.locator('#test-artifacts-dialog [data-artifact-file="capture.png"]').waitFor();
  verify('compact current summaries still expose their complete retained files', daemon.calls.some((call) => call.operation === 'test.artifact.catalog' && call.params.run_id === RUN));
  await page.keyboard.press('Escape');
  daemon.setScenario({ artifactFiles: true, artifactCurrentEmpty: true });
  await page.reload();
  await page.locator('.test-detail>summary').click();
  await page.locator('[data-test-artifacts]').click();
  await dialog.getByLabel('Run', { exact: true }).selectOption(EARLIER_RUN);
  await dialog.locator('img').waitFor();
  await dialog.locator('img').evaluate((image) => image.decode());
  verify('newer nonvisual runs do not hide retained earlier files', await dialog.locator('img').evaluate((image) => image.naturalWidth === 1));
  verify('earlier evidence uses its own run and check', daemon.calls.some((call) => call.operation === 'test.artifact.file' && call.params.run_id === EARLIER_RUN && call.params.check === 'earlier-session'));
  verify('earlier evidence does not inherit the latest test label', /earlier-native-editor/.test(await dialog.locator('.artifact-run-facts').innerText()));
  await dialog.getByLabel('Run', { exact: true }).selectOption(RUN);
  await dialog.getByText('No retained files are available for this run.', { exact: true }).waitFor();
  verify('switching to a run without files clears old screenshots', await dialog.locator('img').count() === 0);
  await dialog.getByRole('button', { name: 'Close evidence files', exact: true }).click();
  verify('no browser errors', errors.length === 0, errors.join('; '));
}
