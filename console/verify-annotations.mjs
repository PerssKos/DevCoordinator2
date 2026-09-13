import path from 'node:path';

export async function verifyAnnotations({ page, daemon, check, scenario, baseUrl, output, theme, viewport }) {
  const verify = (name, success, detail = '') => check(`Annotations ${theme} ${viewport.width}: ${name}`, success, detail);
  daemon.setScenario(scenario);
  await page.addInitScript((selectedTheme) => localStorage.setItem('dc2-theme', selectedTheme), theme);
  await page.goto(`${baseUrl}#/plan/r0123456789abcdef`);
  await page.locator('.plan-context').waitFor();
  daemon.setEvidenceImage(await page.screenshot({ type: 'png' }), viewport.width, viewport.height);
  await page.goto(`${baseUrl}#/tests/t20260101T000100Z-def456`);
  await page.locator('#evidence-image:not([hidden])').waitFor();
  const initialView = await page.evaluate(() => {
    const image = document.querySelector('#evidence-scroll').getBoundingClientRect();
    const name = document.querySelector('#workspace-heading > span');
    return {
      visibleImage: Math.max(0, Math.min(innerHeight, image.bottom) - Math.max(0, image.top)) / innerHeight,
      nameHeight: name.getBoundingClientRect().height,
      nameLineHeight: parseFloat(getComputedStyle(name).lineHeight),
      fullNameAvailable: name.parentElement.title === name.textContent,
    };
  });
  verify('long repository names keep the screenshot visible on arrival', initialView.visibleImage >= 0.2, JSON.stringify(initialView));
  verify('repository heading stays compact with its full name available', initialView.nameHeight <= initialView.nameLineHeight + 1 && initialView.fullNameAvailable, JSON.stringify(initialView));
  const canvas = page.locator('#evidence-canvas');
  const composer = page.locator('#evidence-composer');
  const body = page.locator('#evidence-feedback-create textarea');
  const count = () => canvas.getAttribute('data-draft-count');
  const position = async (offset = 0) => {
    await page.locator('#evidence-scroll').scrollIntoViewIfNeeded();
    const image = await canvas.boundingBox();
    const scroll = await page.locator('#evidence-scroll').boundingBox();
    return { x: Math.max(image.x, scroll.x) + 32 + offset, y: Math.max(image.y, scroll.y) + 36 + offset };
  };
  const useTool = async (tool) => {
    await page.locator(`[data-evidence-tool="${tool}"]`).click();
    return position();
  };
  const cancel = async () => {
    if (await composer.isVisible()) await page.locator('[data-evidence-cancel]').click();
  };
  const draw = async (tool) => {
    const start = await useTool(tool);
    await page.mouse.move(start.x, start.y);
    await page.mouse.down();
    await page.mouse.move(start.x + 64, start.y + 48, { steps: 8 });
    await page.mouse.up();
  };
  const start = await useTool('pin');
  await page.mouse.click(start.x, start.y);
  await body.waitFor({ state: 'visible' });
  verify('pin immediately opens and focuses comment entry', await body.evaluate((element) => document.activeElement === element));
  const box = await composer.boundingBox();
  verify('comment entry is inside the viewport', box.x >= 0 && box.y >= 0 && box.x + box.width <= viewport.width + 1 && box.y + box.height <= viewport.height + 1, JSON.stringify(box));
  const second = await position(28);
  await page.mouse.click(second.x, second.y);
  verify('second empty click moves the pending pin without a duplicate', await count() === '1');
  await body.fill('Keep the pending comment while adjusting the pin.');
  const third = await position(16);
  await page.mouse.click(third.x, third.y);
  verify('reposition preserves entered text', await count() === '1' && await body.inputValue() === 'Keep the pending comment while adjusting the pin.');
  await page.screenshot({ path: path.join(output, `annotation-pin-${theme}-${viewport.width}.png`), mask: [page.locator('#who-email')] });
  await page.screenshot({ path: path.join(output, `annotation-pin-${theme}-${viewport.width}-full.png`), fullPage: true, mask: [page.locator('#who-email')] });
  await page.locator('[data-evidence-next]').click();
  await page.waitForFunction(() => document.querySelector('#evidence-canvas')?.dataset.draftCount === '0');
  verify('image navigation updates the exact screenshot link', new URLSearchParams((await page.url()).split('?').at(-1)).has('image'));
  verify('a different image never inherits another image’s marks', await count() === '0');
  await page.locator('[data-evidence-prev]').click();
  await page.waitForFunction(() => document.querySelector('#evidence-canvas')?.dataset.draftCount === '1');
  verify('returning to an image restores its unsaved comment and marks', await body.inputValue() === 'Keep the pending comment while adjusting the pin.');
  await cancel();
  verify('Cancel removes unsaved feedback and restores canvas focus', await count() === '0' && await canvas.evaluate((element) => document.activeElement === element));
  for (const tool of ['rectangle', 'arrow', 'freehand', 'highlight']) {
    await draw(tool);
    await body.waitFor({ state: 'visible' });
    verify(`${tool} completes with immediate comment entry`, await count() === '1' && await body.evaluate((element) => document.activeElement === element));
    await page.locator('[data-evidence-undo]').click();
    verify(`${tool} can be undone`, await count() === '0');
    await page.locator('[data-evidence-redo]').click();
    verify(`${tool} can be restored`, await count() === '1');
    await cancel();
    const origin = await useTool(tool);
    await page.mouse.move(origin.x, origin.y); await page.mouse.down();
    await page.mouse.move(origin.x + 50, origin.y + 30, { steps: 4 });
    await canvas.dispatchEvent('pointercancel', { pointerId: 1 });
    await page.mouse.up();
    verify(`${tool} cancelled pointer gesture does not create a mark`, await count() === '0');
  }
  const labelPosition = await useTool('text');
  await page.mouse.click(labelPosition.x, labelPosition.y);
  verify('text tool immediately focuses its label input', await page.locator('.evidence-text-entry').evaluate(element => document.activeElement === element));
  await page.locator('.evidence-text-entry').fill('X');
  await page.locator('.evidence-label-editor button[type=submit]').click();
  verify('text labels have a visible Add action and immediate feedback entry', await count() === '1' && await body.evaluate((element) => document.activeElement === element));
  await cancel();
  const cancelLabelPosition = await useTool('text');
  await page.mouse.click(cancelLabelPosition.x, cancelLabelPosition.y);
  await page.locator('.evidence-text-entry').fill('Discard this label');
  await page.locator('[data-evidence-next]').click();
  await page.waitForFunction(() => !document.querySelector('.evidence-label-editor'));
  await page.locator('[data-evidence-prev]').click();
  await page.locator('.evidence-text-entry').waitFor();
  verify('unfinished text labels remain attached to their original image', await page.locator('.evidence-text-entry').inputValue() === 'Discard this label');
  await page.locator('.evidence-text-entry').focus();
  await page.keyboard.press('Escape');
  verify('Escape cancels an unfinished text label without adding a mark', await count() === '0' && await page.locator('.evidence-text-entry').count() === 0);
  await draw('rectangle');
  await page.locator('[data-evidence-tool=select]').click();
  await page.locator('#evidence-color').selectOption('#ef4444');
  await canvas.focus();
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('Delete');
  verify('Select supports keyboard adjustment and deletion of the draft', await count() === '0');
  await page.locator('[data-evidence-undo]').click();
  await page.locator('[data-evidence-zoom-in]').click();
  verify('zoom changes the rendered image scale', await page.locator('#evidence-zoom-value').innerText() === '125%');
  await page.locator('[data-evidence-zoom-out]').click();
  await page.locator('[data-evidence-fit]').click();
  verify('Fit restores the image scale', await page.locator('#evidence-zoom-value').innerText() === '100%');
  await page.locator('[data-evidence-compose]').click();
  await page.keyboard.press('Escape');
  verify('Escape dismisses comment entry without discarding the draft', !await composer.isVisible() && await count() === '1');
  await page.locator('[data-evidence-compose]').click();
  await body.fill('Keep this exact image and red rectangle with the feedback.');
  const create = page.locator('#evidence-feedback-create button[type=submit]');
  await page.route('**/api/v2/test.evidence.feedback.create', (route) => route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ ok: false, error: { code: 'unavailable', message: 'Fixture save unavailable' } }) }));
  await create.click();
  await page.locator('#evidence-feedback-error:not([hidden])').waitFor();
  verify('failed save keeps the comment, marks, and retry action', await count() === '1' && (await body.inputValue()).includes('exact image') && await create.isEnabled());
  await page.unroute('**/api/v2/test.evidence.feedback.create');
  await create.click();
  await page.locator('.evidence-thread').waitFor();
  const saved = daemon.calls.filter((call) => call.operation === 'test.evidence.feedback.create').at(-1)?.params;
  verify('successful save sends the selected mark color and exact image', saved?.marks?.[0]?.color === '#ef4444' && !!saved?.image_id && saved.body.includes('exact image'));
  verify('save clears only the submitted draft and reveals the discussion', await count() === '0' && await page.locator('.evidence-thread').isVisible());
  await page.reload();
  await page.locator('#evidence-image:not([hidden])').waitFor();
  if (!await page.locator('.evidence-thread-summary').first().isVisible()) await page.locator('.evidence-mobile-inspector-toggle').click();
  await page.locator('.evidence-thread-summary').first().click();
  verify('saved feedback remains accessible after reload', (await page.locator('.evidence-thread').innerText()).includes('exact image'));
  await page.locator('[data-evidence-delete]').click();
  await page.waitForFunction(() => !document.querySelector('.evidence-thread'));
  verify('Delete removes only the fixture’s saved annotation', daemon.calls.some((call) => call.operation === 'test.evidence.feedback.delete'));
  await page.goto(`${baseUrl}#/tests/t20260101T000100Z-def456?image=unavailable-image`);
  await page.getByText('This screenshot is not available for this run.').waitFor();
  verify('an unavailable image link never silently opens a different screenshot', await page.locator('#evidence-canvas').count() === 0);
}
